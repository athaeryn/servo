/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use cookie_rs;
use devtools_traits::HttpRequest as DevtoolsHttpRequest;
use devtools_traits::HttpResponse as DevtoolsHttpResponse;
use devtools_traits::{ChromeToDevtoolsControlMsg, DevtoolsControlMsg, NetworkEvent};
use flate2::Compression;
use flate2::write::{GzEncoder, DeflateEncoder};
use hyper::header::{Headers, Location, ContentLength, Host};
use hyper::http::RawStatus;
use hyper::method::Method;
use hyper::status::StatusCode;
use net::cookie::Cookie;
use net::cookie_storage::CookieStorage;
use net::hsts::{HSTSList};
use net::http_loader::{load, LoadError, HttpRequestFactory, HttpRequest, HttpResponse};
use net_traits::{LoadData, CookieSource};
use std::borrow::Cow;
use std::io::{self, Write, Read, Cursor};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, mpsc, RwLock};
use url::Url;

const DEFAULT_USER_AGENT: &'static str = "Test-agent";

fn respond_with(body: Vec<u8>) -> MockResponse {
    let mut headers = Headers::new();
    respond_with_headers(body, &mut headers)
}

fn respond_with_headers(body: Vec<u8>, headers: &mut Headers) -> MockResponse {
    headers.set(ContentLength(body.len() as u64));

    MockResponse::new(
        headers.clone(),
        StatusCode::Ok,
        RawStatus(200, Cow::Borrowed("Ok")),
        body
    )
}

fn read_response(reader: &mut Read) -> String {
    let mut buf = vec![0; 1024];
    match reader.read(&mut buf) {
        Ok(len) if len > 0 => {
            unsafe { buf.set_len(len); }
            String::from_utf8(buf).unwrap()
        },
        Ok(_) => "".to_string(),
        Err(e) => panic!("problem reading response {}", e)
    }
}

struct MockResponse {
    h: Headers,
    sc: StatusCode,
    sr: RawStatus,
    msg: Cursor<Vec<u8>>
}

impl MockResponse {
    fn new(h: Headers, sc: StatusCode, sr: RawStatus, msg: Vec<u8>) -> MockResponse {
        MockResponse { h: h, sc: sc, sr: sr, msg: Cursor::new(msg) }
    }
}

impl Read for MockResponse {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.msg.read(buf)
    }
}

impl HttpResponse for MockResponse {
    fn headers(&self) -> &Headers { &self.h }
    fn status(&self) -> StatusCode { self.sc }
    fn status_raw(&self) -> &RawStatus { &self.sr }
}

fn redirect_to(host: String) -> MockResponse {
    let mut headers = Headers::new();
    headers.set(Location(host.to_string()));

    MockResponse::new(
        headers,
        StatusCode::MovedPermanently,
        RawStatus(301, Cow::Borrowed("Moved Permanently")),
        <[_]>::to_vec("".as_bytes())
    )
}


enum ResponseType {
    Redirect(String),
    Text(Vec<u8>),
    WithHeaders(Vec<u8>, Headers)
}

struct MockRequest {
    headers: Headers,
    t: ResponseType
}

impl MockRequest {
    fn new(t: ResponseType) -> MockRequest {
        MockRequest { headers: Headers::new(), t: t }
    }
}

fn response_for_request_type(t: ResponseType) -> Result<MockResponse, LoadError> {
    match t {
        ResponseType::Redirect(location) => {
            Ok(redirect_to(location))
        },
        ResponseType::Text(b) => {
            Ok(respond_with(b))
        },
        ResponseType::WithHeaders(b, mut h) => {
            Ok(respond_with_headers(b, &mut h))
        }
    }
}

impl HttpRequest for MockRequest {
    type R = MockResponse;

    fn headers_mut(&mut self) -> &mut Headers { &mut self.headers }

    fn send(self, _: &Option<Vec<u8>>) -> Result<MockResponse, LoadError> {
        response_for_request_type(self.t)
    }
}

struct AssertRequestMustHaveHeaders {
    expected_headers: Headers,
    request_headers: Headers,
    t: ResponseType
}

impl AssertRequestMustHaveHeaders {
    fn new(t: ResponseType, expected_headers: Headers) -> Self {
        AssertRequestMustHaveHeaders { expected_headers: expected_headers, request_headers: Headers::new(), t: t }
    }
}

impl HttpRequest for AssertRequestMustHaveHeaders {
    type R = MockResponse;

    fn headers_mut(&mut self) -> &mut Headers { &mut self.request_headers }

    fn send(self, _: &Option<Vec<u8>>) -> Result<MockResponse, LoadError> {
        for header in self.expected_headers.iter() {
            assert!(self.request_headers.get_raw(header.name()).is_some());
            assert_eq!(
                self.request_headers.get_raw(header.name()).unwrap(),
                self.expected_headers.get_raw(header.name()).unwrap()
            )
        }

        response_for_request_type(self.t)
    }
}

struct AssertMustHaveHeadersRequestFactory {
    expected_headers: Headers,
    body: Vec<u8>
}

impl HttpRequestFactory for AssertMustHaveHeadersRequestFactory {
    type R = AssertRequestMustHaveHeaders;

    fn create(&self, _: Url, _: Method) -> Result<AssertRequestMustHaveHeaders, LoadError> {
        Ok(
            AssertRequestMustHaveHeaders::new(
                ResponseType::Text(self.body.clone()),
                self.expected_headers.clone()
            )
        )
    }
}

fn assert_cookie_for_domain(cookie_jar: Arc<RwLock<CookieStorage>>, domain: &str, cookie: &str) {
    let mut cookie_jar = cookie_jar.write().unwrap();
    let url = Url::parse(&*domain).unwrap();
    let cookies = cookie_jar.cookies_for_url(&url, CookieSource::HTTP);

    if let Some(cookie_list) = cookies {
        assert_eq!(cookie.to_string(), cookie_list);
    } else {
        assert_eq!(cookie.len(), 0);
    }
}

struct AssertMustHaveBodyRequest {
    expected_body: Option<Vec<u8>>,
    headers: Headers,
    t: ResponseType
}

impl AssertMustHaveBodyRequest {
    fn new(t: ResponseType, expected_body: Option<Vec<u8>>) -> Self {
        AssertMustHaveBodyRequest { expected_body: expected_body, headers: Headers::new(), t: t }
    }
}

impl HttpRequest for AssertMustHaveBodyRequest {
    type R = MockResponse;

    fn headers_mut(&mut self) -> &mut Headers { &mut self.headers }

    fn send(self, body: &Option<Vec<u8>>) -> Result<MockResponse, LoadError> {
        assert_eq!(self.expected_body, *body);

        response_for_request_type(self.t)
    }
}

fn expect_devtools_http_request(devtools_port: &Receiver<DevtoolsControlMsg>) -> DevtoolsHttpRequest {
    match devtools_port.recv().unwrap() {
        DevtoolsControlMsg::FromChrome(
        ChromeToDevtoolsControlMsg::NetworkEvent(_, net_event)) => {
            match net_event {
                NetworkEvent::HttpRequest(httprequest) => {
                    httprequest
                },

                _ => panic!("No HttpRequest Received"),
            }
        },
        _ => panic!("No HttpRequest Received"),
    }
}

fn expect_devtools_http_response(devtools_port: &Receiver<DevtoolsControlMsg>) -> DevtoolsHttpResponse {
    match devtools_port.recv().unwrap() {
        DevtoolsControlMsg::FromChrome(
            ChromeToDevtoolsControlMsg::NetworkEvent(_, net_event_response)) => {
            match net_event_response {
                NetworkEvent::HttpResponse(httpresponse) => {
                    httpresponse
                },

                _ => panic!("No HttpResponse Received"),
            }
        },
        _ => panic!("No HttpResponse Received"),
    }
}

#[test]
fn test_load_when_request_is_not_get_or_head_and_there_is_no_body_content_length_should_be_set_to_0() {
    let url = Url::parse("http://mozilla.com").unwrap();

    let hsts_list = Arc::new(RwLock::new(HSTSList::new()));
    let cookie_jar = Arc::new(RwLock::new(CookieStorage::new()));

    let mut load_data = LoadData::new(url.clone(), None);
    load_data.data = None;
    load_data.method = Method::Post;

    let mut content_length = Headers::new();
    content_length.set_raw("Content-Length".to_owned(), vec![<[_]>::to_vec("0".as_bytes())]);

    let _ = load::<AssertRequestMustHaveHeaders>(
        load_data.clone(), hsts_list, cookie_jar, None,
        &AssertMustHaveHeadersRequestFactory {
            expected_headers: content_length,
            body: <[_]>::to_vec(&[])
        }, DEFAULT_USER_AGENT.to_string());
}

#[test]
fn test_request_and_response_data_with_network_messages() {
    struct Factory;

    impl HttpRequestFactory for Factory {
        type R = MockRequest;

        fn create(&self, _: Url, _: Method) -> Result<MockRequest, LoadError> {
            let mut headers = Headers::new();
            headers.set(Host { hostname: "foo.bar".to_owned(), port: None });
            Ok(MockRequest::new(
                   ResponseType::WithHeaders(<[_]>::to_vec("Yay!".as_bytes()), headers))
            )
        }
    }

    let hsts_list = Arc::new(RwLock::new(HSTSList::new()));
    let cookie_jar = Arc::new(RwLock::new(CookieStorage::new()));

    let url = Url::parse("https://mozilla.com").unwrap();
    let (devtools_chan, devtools_port) = mpsc::channel::<DevtoolsControlMsg>();
    let mut load_data = LoadData::new(url.clone(), None);
    let mut request_headers = Headers::new();
    request_headers.set(Host { hostname: "bar.foo".to_owned(), port: None });
    load_data.headers = request_headers.clone();
    let _ = load::<MockRequest>(load_data, hsts_list, cookie_jar, Some(devtools_chan), &Factory,
                                DEFAULT_USER_AGENT.to_string());

    // notification received from devtools
    let devhttprequest = expect_devtools_http_request(&devtools_port);
    let devhttpresponse = expect_devtools_http_response(&devtools_port);

    let httprequest = DevtoolsHttpRequest {
        url: url,
        method: Method::Get,
        headers: request_headers,
        body: None,
    };

    let content = "Yay!";
    let mut response_headers = Headers::new();
    response_headers.set(ContentLength(content.len() as u64));
    response_headers.set(Host { hostname: "foo.bar".to_owned(), port: None });

    let httpresponse = DevtoolsHttpResponse {
        headers: Some(response_headers),
        status: Some(RawStatus(200, Cow::Borrowed("Ok"))),
        body: None
    };

    assert_eq!(devhttprequest, httprequest);
    assert_eq!(devhttpresponse, httpresponse);
}

#[test]
fn test_load_when_redirecting_from_a_post_should_rewrite_next_request_as_get() {
    struct Factory;

    impl HttpRequestFactory for Factory {
        type R = MockRequest;

        fn create(&self, url: Url, method: Method) -> Result<MockRequest, LoadError> {
            if url.domain().unwrap() == "mozilla.com" {
                assert_eq!(Method::Post, method);
                Ok(MockRequest::new(ResponseType::Redirect("http://mozilla.org".to_string())))
            } else {
                assert_eq!(Method::Get, method);
                Ok(MockRequest::new(ResponseType::Text(<[_]>::to_vec("Yay!".as_bytes()))))
            }
        }
    }

    let url = Url::parse("http://mozilla.com").unwrap();
    let mut load_data = LoadData::new(url.clone(), None);
    load_data.method = Method::Post;

    let hsts_list = Arc::new(RwLock::new(HSTSList::new()));
    let cookie_jar = Arc::new(RwLock::new(CookieStorage::new()));

    let _ = load::<MockRequest>(load_data, hsts_list, cookie_jar, None, &Factory, DEFAULT_USER_AGENT.to_string());
}

#[test]
fn test_load_should_decode_the_response_as_deflate_when_response_headers_have_content_encoding_deflate() {
    struct Factory;

    impl HttpRequestFactory for Factory {
        type R = MockRequest;

        fn create(&self, _: Url, _: Method) -> Result<MockRequest, LoadError> {
            let mut e = DeflateEncoder::new(Vec::new(), Compression::Default);
            e.write(b"Yay!").unwrap();
            let encoded_content = e.finish().unwrap();

            let mut headers = Headers::new();
            headers.set_raw("Content-Encoding", vec![b"deflate".to_vec()]);
            Ok(MockRequest::new(ResponseType::WithHeaders(encoded_content, headers)))
        }
    }

    let url = Url::parse("http://mozilla.com").unwrap();
    let load_data = LoadData::new(url.clone(), None);

    let hsts_list = Arc::new(RwLock::new(HSTSList::new()));
    let cookie_jar = Arc::new(RwLock::new(CookieStorage::new()));

    let mut response = load::<MockRequest>(
        load_data, hsts_list, cookie_jar, None,
        &Factory,
        DEFAULT_USER_AGENT.to_string())
        .unwrap();

    assert_eq!(read_response(&mut response), "Yay!");
}

#[test]
fn test_load_should_decode_the_response_as_gzip_when_response_headers_have_content_encoding_gzip() {
    struct Factory;

    impl HttpRequestFactory for Factory {
        type R = MockRequest;

        fn create(&self, _: Url, _: Method) -> Result<MockRequest, LoadError> {
            let mut e = GzEncoder::new(Vec::new(), Compression::Default);
            e.write(b"Yay!").unwrap();
            let encoded_content = e.finish().unwrap();

            let mut headers = Headers::new();
            headers.set_raw("Content-Encoding", vec![b"gzip".to_vec()]);
            Ok(MockRequest::new(ResponseType::WithHeaders(encoded_content, headers)))
        }
    }

    let url = Url::parse("http://mozilla.com").unwrap();
    let load_data = LoadData::new(url.clone(), None);
    let hsts_list = Arc::new(RwLock::new(HSTSList::new()));
    let cookie_jar = Arc::new(RwLock::new(CookieStorage::new()));

    let mut response = load::<MockRequest>(
        load_data,
        hsts_list,
        cookie_jar,
        None, &Factory,
        DEFAULT_USER_AGENT.to_string())
        .unwrap();

    assert_eq!(read_response(&mut response), "Yay!");
}

#[test]
fn test_load_doesnt_send_request_body_on_any_redirect() {
    struct Factory;

    impl HttpRequestFactory for Factory {
        type R = AssertMustHaveBodyRequest;

        fn create(&self, url: Url, _: Method) -> Result<AssertMustHaveBodyRequest, LoadError> {
            if url.domain().unwrap() == "mozilla.com" {
                Ok(
                    AssertMustHaveBodyRequest::new(
                        ResponseType::Redirect("http://mozilla.org".to_string()),
                        Some(<[_]>::to_vec("Body on POST!".as_bytes()))
                    )
                )
            } else {
                Ok(
                    AssertMustHaveBodyRequest::new(
                        ResponseType::Text(<[_]>::to_vec("Yay!".as_bytes())),
                        None
                    )
                )
            }
        }
    }

    let url = Url::parse("http://mozilla.com").unwrap();
    let mut load_data = LoadData::new(url.clone(), None);
    load_data.data = Some(<[_]>::to_vec("Body on POST!".as_bytes()));

    let hsts_list = Arc::new(RwLock::new(HSTSList::new()));
    let cookie_jar = Arc::new(RwLock::new(CookieStorage::new()));

    let _ = load::<AssertMustHaveBodyRequest>(
        load_data, hsts_list, cookie_jar,
        None,
        &Factory,
        DEFAULT_USER_AGENT.to_string());
}

#[test]
fn test_load_doesnt_add_host_to_sts_list_when_url_is_http_even_if_sts_headers_are_present() {
    struct Factory;

    impl HttpRequestFactory for Factory {
        type R = MockRequest;

        fn create(&self, _: Url, _: Method) -> Result<MockRequest, LoadError> {
            let content = <[_]>::to_vec("Yay!".as_bytes());
            let mut headers = Headers::new();
            headers.set_raw("Strict-Transport-Security", vec![b"max-age=31536000".to_vec()]);
            Ok(MockRequest::new(ResponseType::WithHeaders(content, headers)))
        }
    }

    let url = Url::parse("http://mozilla.com").unwrap();

    let load_data = LoadData::new(url.clone(), None);

    let hsts_list = Arc::new(RwLock::new(HSTSList::new()));
    let cookie_jar = Arc::new(RwLock::new(CookieStorage::new()));

    let _ = load::<MockRequest>(load_data,
                                hsts_list.clone(),
                                cookie_jar,
                                None,
                                &Factory,
                                DEFAULT_USER_AGENT.to_string());

    assert_eq!(hsts_list.read().unwrap().is_host_secure("mozilla.com"), false);
}

#[test]
fn test_load_adds_host_to_sts_list_when_url_is_https_and_sts_headers_are_present() {
    struct Factory;

    impl HttpRequestFactory for Factory {
        type R = MockRequest;

        fn create(&self, _: Url, _: Method) -> Result<MockRequest, LoadError> {
            let content = <[_]>::to_vec("Yay!".as_bytes());
            let mut headers = Headers::new();
            headers.set_raw("Strict-Transport-Security", vec![b"max-age=31536000".to_vec()]);
            Ok(MockRequest::new(ResponseType::WithHeaders(content, headers)))
        }
    }

    let url = Url::parse("https://mozilla.com").unwrap();

    let load_data = LoadData::new(url.clone(), None);

    let hsts_list = Arc::new(RwLock::new(HSTSList::new()));
    let cookie_jar = Arc::new(RwLock::new(CookieStorage::new()));

    let _ = load::<MockRequest>(load_data,
                                hsts_list.clone(),
                                cookie_jar,
                                None,
                                &Factory,
                                DEFAULT_USER_AGENT.to_string());

    assert!(hsts_list.read().unwrap().is_host_secure("mozilla.com"));
}

#[test]
fn test_load_sets_cookies_in_the_resource_manager_when_it_get_set_cookie_header_in_response() {
    struct Factory;

    impl HttpRequestFactory for Factory {
        type R = MockRequest;

        fn create(&self, _: Url, _: Method) -> Result<MockRequest, LoadError> {
            let content = <[_]>::to_vec("Yay!".as_bytes());
            let mut headers = Headers::new();
            headers.set_raw("set-cookie", vec![b"mozillaIs=theBest".to_vec()]);
            Ok(MockRequest::new(ResponseType::WithHeaders(content, headers)))
        }
    }

    let url = Url::parse("http://mozilla.com").unwrap();

    let hsts_list = Arc::new(RwLock::new(HSTSList::new()));
    let cookie_jar = Arc::new(RwLock::new(CookieStorage::new()));

    assert_cookie_for_domain(cookie_jar.clone(), "http://mozilla.com", "");

    let load_data = LoadData::new(url.clone(), None);

    let _ = load::<MockRequest>(load_data,
                                hsts_list,
                                cookie_jar.clone(),
                                None,
                                &Factory,
                                DEFAULT_USER_AGENT.to_string());

    assert_cookie_for_domain(cookie_jar.clone(), "http://mozilla.com", "mozillaIs=theBest");
}

#[test]
fn test_load_sets_requests_cookies_header_for_url_by_getting_cookies_from_the_resource_manager() {
    let url = Url::parse("http://mozilla.com").unwrap();

    let mut load_data = LoadData::new(url.clone(), None);
    load_data.data = Some(<[_]>::to_vec("Yay!".as_bytes()));

    let hsts_list = Arc::new(RwLock::new(HSTSList::new()));
    let cookie_jar = Arc::new(RwLock::new(CookieStorage::new()));

    {
        let mut cookie_jar = cookie_jar.write().unwrap();
        let cookie_url = url.clone();
        let cookie = Cookie::new_wrapped(
            cookie_rs::Cookie::parse("mozillaIs=theBest").unwrap(),
            &cookie_url,
            CookieSource::HTTP
        ).unwrap();
        cookie_jar.push(cookie, CookieSource::HTTP);
    }

    let mut cookie = Headers::new();
    cookie.set_raw("Cookie".to_owned(), vec![<[_]>::to_vec("mozillaIs=theBest".as_bytes())]);

    let _ = load::<AssertRequestMustHaveHeaders>(
        load_data.clone(), hsts_list, cookie_jar, None,
        &AssertMustHaveHeadersRequestFactory {
            expected_headers: cookie,
            body: <[_]>::to_vec(&*load_data.data.unwrap())
        }, DEFAULT_USER_AGENT.to_string());
}

#[test]
fn test_load_sets_content_length_to_length_of_request_body() {
    let content = "This is a request body";

    let url = Url::parse("http://mozilla.com").unwrap();
    let mut load_data = LoadData::new(url.clone(), None);
    load_data.data = Some(<[_]>::to_vec(content.as_bytes()));

    let mut content_len_headers = Headers::new();
    let content_len = format!("{}", content.len());
    content_len_headers.set_raw(
        "Content-Length".to_owned(), vec![<[_]>::to_vec(&*content_len.as_bytes())]
    );

    let hsts_list = Arc::new(RwLock::new(HSTSList::new()));
    let cookie_jar = Arc::new(RwLock::new(CookieStorage::new()));

    let _ = load::<AssertRequestMustHaveHeaders>(
        load_data.clone(), hsts_list, cookie_jar, None,
        &AssertMustHaveHeadersRequestFactory {
            expected_headers: content_len_headers,
            body: <[_]>::to_vec(&*load_data.data.unwrap())
        }, DEFAULT_USER_AGENT.to_string());
}

#[test]
fn test_load_uses_explicit_accept_from_headers_in_load_data() {
    let mut accept_headers = Headers::new();
    accept_headers.set_raw("Accept".to_owned(), vec![b"text/html".to_vec()]);

    let url = Url::parse("http://mozilla.com").unwrap();
    let mut load_data = LoadData::new(url.clone(), None);
    load_data.data = Some(<[_]>::to_vec("Yay!".as_bytes()));
    load_data.headers.set_raw("Accept".to_owned(), vec![b"text/html".to_vec()]);

    let hsts_list = Arc::new(RwLock::new(HSTSList::new()));
    let cookie_jar = Arc::new(RwLock::new(CookieStorage::new()));

    let _ = load::<AssertRequestMustHaveHeaders>(load_data,
                                                 hsts_list,
                                                 cookie_jar,
                                                 None,
                                                 &AssertMustHaveHeadersRequestFactory {
                                                    expected_headers: accept_headers,
                                                    body: <[_]>::to_vec("Yay!".as_bytes())
                                                }, DEFAULT_USER_AGENT.to_string());
}

#[test]
fn test_load_sets_default_accept_to_html_xhtml_xml_and_then_anything_else() {
    let mut accept_headers = Headers::new();
    accept_headers.set_raw(
        "Accept".to_owned(), vec![b"text/html, application/xhtml+xml, application/xml; q=0.9, */*; q=0.8".to_vec()]
    );

    let url = Url::parse("http://mozilla.com").unwrap();
    let mut load_data = LoadData::new(url.clone(), None);
    load_data.data = Some(<[_]>::to_vec("Yay!".as_bytes()));

    let hsts_list = Arc::new(RwLock::new(HSTSList::new()));
    let cookie_jar = Arc::new(RwLock::new(CookieStorage::new()));

    let _ = load::<AssertRequestMustHaveHeaders>(load_data,
                                                 hsts_list,
                                                 cookie_jar,
                                                 None,
                                                 &AssertMustHaveHeadersRequestFactory {
                                                     expected_headers: accept_headers,
                                                     body: <[_]>::to_vec("Yay!".as_bytes())
                                                 }, DEFAULT_USER_AGENT.to_string());
}

#[test]
fn test_load_uses_explicit_accept_encoding_from_load_data_headers() {
    let mut accept_encoding_headers = Headers::new();
    accept_encoding_headers.set_raw("Accept-Encoding".to_owned(), vec![b"chunked".to_vec()]);

    let url = Url::parse("http://mozilla.com").unwrap();
    let mut load_data = LoadData::new(url.clone(), None);
    load_data.data = Some(<[_]>::to_vec("Yay!".as_bytes()));
    load_data.headers.set_raw("Accept-Encoding".to_owned(), vec![b"chunked".to_vec()]);

    let hsts_list = Arc::new(RwLock::new(HSTSList::new()));
    let cookie_jar = Arc::new(RwLock::new(CookieStorage::new()));

    let _ = load::<AssertRequestMustHaveHeaders>(load_data,
                                                 hsts_list,
                                                 cookie_jar,
                                                 None,
                                                 &AssertMustHaveHeadersRequestFactory {
                                                     expected_headers: accept_encoding_headers,
                                                     body: <[_]>::to_vec("Yay!".as_bytes())
                                                 }, DEFAULT_USER_AGENT.to_string());
}

#[test]
fn test_load_sets_default_accept_encoding_to_gzip_and_deflate() {
    let mut accept_encoding_headers = Headers::new();
    accept_encoding_headers.set_raw("Accept-Encoding".to_owned(), vec![b"gzip, deflate".to_vec()]);

    let url = Url::parse("http://mozilla.com").unwrap();
    let mut load_data = LoadData::new(url.clone(), None);
    load_data.data = Some(<[_]>::to_vec("Yay!".as_bytes()));

    let hsts_list = Arc::new(RwLock::new(HSTSList::new()));
    let cookie_jar = Arc::new(RwLock::new(CookieStorage::new()));

    let _ = load::<AssertRequestMustHaveHeaders>(load_data,
                                                 hsts_list,
                                                 cookie_jar,
                                                 None,
                                                 &AssertMustHaveHeadersRequestFactory {
                                                     expected_headers: accept_encoding_headers,
                                                     body: <[_]>::to_vec("Yay!".as_bytes())
                                                 }, DEFAULT_USER_AGENT.to_string());
}

#[test]
fn test_load_errors_when_there_a_redirect_loop() {
    struct Factory;

    impl HttpRequestFactory for Factory {
        type R = MockRequest;

        fn create(&self, url: Url, _: Method) -> Result<MockRequest, LoadError> {
            if url.domain().unwrap() == "mozilla.com" {
                Ok(MockRequest::new(ResponseType::Redirect("http://mozilla.org".to_string())))
            } else if url.domain().unwrap() == "mozilla.org" {
                Ok(MockRequest::new(ResponseType::Redirect("http://mozilla.com".to_string())))
            } else {
                panic!("unexpected host {:?}", url)
            }
        }
    }

    let url = Url::parse("http://mozilla.com").unwrap();
    let load_data = LoadData::new(url.clone(), None);

    let hsts_list = Arc::new(RwLock::new(HSTSList::new()));
    let cookie_jar = Arc::new(RwLock::new(CookieStorage::new()));

    match load::<MockRequest>(load_data, hsts_list, cookie_jar, None, &Factory, DEFAULT_USER_AGENT.to_string()) {
        Err(LoadError::InvalidRedirect(_, msg)) => {
            assert_eq!(msg, "redirect loop");
        },
        _ => panic!("expected max redirects to fail")
    }
}

#[test]
fn test_load_errors_when_there_is_too_many_redirects() {
    struct Factory;

    impl HttpRequestFactory for Factory {
        type R = MockRequest;

        fn create(&self, url: Url, _: Method) -> Result<MockRequest, LoadError> {
            if url.domain().unwrap() == "mozilla.com" {
                Ok(MockRequest::new(ResponseType::Redirect(format!("{}/1", url.serialize()))))
            } else {
                panic!("unexpected host {:?}", url)
            }
        }
    }

    let url = Url::parse("http://mozilla.com").unwrap();
    let load_data = LoadData::new(url.clone(), None);

    let hsts_list = Arc::new(RwLock::new(HSTSList::new()));
    let cookie_jar = Arc::new(RwLock::new(CookieStorage::new()));

    match load::<MockRequest>(load_data, hsts_list, cookie_jar, None, &Factory, DEFAULT_USER_AGENT.to_string()) {
        Err(LoadError::MaxRedirects(url)) => {
            assert_eq!(url.domain().unwrap(), "mozilla.com")
        },
        _ => panic!("expected max redirects to fail")
    }
}

#[test]
fn test_load_follows_a_redirect() {
    struct Factory;

    impl HttpRequestFactory for Factory {
        type R = MockRequest;

        fn create(&self, url: Url, _: Method) -> Result<MockRequest, LoadError> {
            if url.domain().unwrap() == "mozilla.com" {
                Ok(MockRequest::new(ResponseType::Redirect("http://mozilla.org".to_string())))
            } else if url.domain().unwrap() == "mozilla.org" {
                Ok(
                    MockRequest::new(
                        ResponseType::Text(
                            <[_]>::to_vec("Yay!".as_bytes())
                        )
                    )
                )
            } else {
                panic!("unexpected host {:?}", url)
            }
        }
    }

    let url = Url::parse("http://mozilla.com").unwrap();
    let load_data = LoadData::new(url.clone(), None);

    let hsts_list = Arc::new(RwLock::new(HSTSList::new()));
    let cookie_jar = Arc::new(RwLock::new(CookieStorage::new()));

    match load::<MockRequest>(load_data, hsts_list, cookie_jar, None, &Factory, DEFAULT_USER_AGENT.to_string()) {
        Err(e) => panic!("expected to follow a redirect {:?}", e),
        Ok(mut lr) => {
            let response = read_response(&mut lr);
            assert_eq!(response, "Yay!".to_string());
        }
    }
}

struct DontConnectFactory;

impl HttpRequestFactory for DontConnectFactory {
    type R = MockRequest;

    fn create(&self, url: Url, _: Method) -> Result<MockRequest, LoadError> {
        Err(LoadError::Connection(url, "should not have connected".to_string()))
    }
}

#[test]
fn test_load_errors_when_scheme_is_not_http_or_https() {
    let url = Url::parse("ftp://not-supported").unwrap();
    let load_data = LoadData::new(url.clone(), None);

    let hsts_list = Arc::new(RwLock::new(HSTSList::new()));
    let cookie_jar = Arc::new(RwLock::new(CookieStorage::new()));

    match load::<MockRequest>(load_data,
                              hsts_list,
                              cookie_jar,
                              None,
                              &DontConnectFactory,
                              DEFAULT_USER_AGENT.to_string()) {
        Err(LoadError::UnsupportedScheme(_)) => {}
        _ => panic!("expected ftp scheme to be unsupported")
    }
}

#[test]
fn test_load_errors_when_viewing_source_and_inner_url_scheme_is_not_http_or_https() {
    let url = Url::parse("view-source:ftp://not-supported").unwrap();
    let load_data = LoadData::new(url.clone(), None);

    let hsts_list = Arc::new(RwLock::new(HSTSList::new()));
    let cookie_jar = Arc::new(RwLock::new(CookieStorage::new()));

    match load::<MockRequest>(load_data,
                              hsts_list,
                              cookie_jar,
                              None,
                              &DontConnectFactory,
                              DEFAULT_USER_AGENT.to_string()) {
        Err(LoadError::UnsupportedScheme(_)) => {}
        _ => panic!("expected ftp scheme to be unsupported")
    }
}
