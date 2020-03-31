//! A basic API client with standard kube error handling

use crate::config::ClientConfig;
use crate::{Error, ErrorResponse, Result};
use bytes::Bytes;
use either::{Either, Left, Right};
use futures::{self, Stream, TryStream, TryStreamExt};
use http::{self, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::{self, Value};

// TODO: replace with Status in k8s openapi?

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StatusDetails {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub group: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub uid: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub causes: Vec<StatusCause>,
    #[serde(default, skip_serializing_if = "num::Zero::is_zero")]
    pub retry_after_seconds: u32,
}

#[derive(Deserialize, Debug)]
pub struct StatusCause {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub field: String,
}

#[derive(Deserialize, Debug)]
pub struct Status {
    // TODO: typemeta
    // TODO: metadata that can be completely empty (listmeta...)
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<StatusDetails>,
    #[serde(default, skip_serializing_if = "num::Zero::is_zero")]
    pub code: u16,
}

/// Client for connecting with a kubernetes cluster.
#[derive(Clone)]
pub struct Client {
    cluster_url: reqwest::Url,
    inner: reqwest::Client,
}

impl Client {
    /// Create a client based on a [`ClientConfig`]
    pub async fn new(client_config: ClientConfig) -> Result<Self> {
        let cluster_url = client_config.cluster_url.clone();
        let builder: reqwest::ClientBuilder = client_config.into();
        Ok(Self {
            cluster_url,
            inner: builder.build()?,
        })
    }

    /// Create and initialize a [`Client`] using the inferred
    /// configuration.
    ///
    /// Will use [`ClientConfig::infer`] to try in-cluster enironment
    /// variables first, then fallback to the local kube config.
    ///
    /// Will fail if neither configuration could be loaded.
    pub async fn default() -> Result<Self> {
        let client_config = ClientConfig::infer().await?;
        Self::new(client_config).await
    }

    async fn send(&self, request: http::Request<Vec<u8>>) -> Result<reqwest::Response> {
        let (parts, body) = request.into_parts();
        let uri_str = format!("{}{}", self.cluster_url, parts.uri);
        trace!("Sending request => method = {} uri = {}", parts.method, uri_str);

        let request = match parts.method {
            http::Method::GET
            | http::Method::POST
            | http::Method::DELETE
            | http::Method::PUT
            | http::Method::PATCH => self.inner.request(parts.method, &uri_str),
            other => return Err(Error::InvalidMethod(other.to_string())),
        };

        let req = request.headers(parts.headers).body(body).build()?;
        let res = self.inner.execute(req).await?;
        Ok(res)
    }

    pub async fn request<T>(&self, request: http::Request<Vec<u8>>) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let res: reqwest::Response = self.send(request).await?;
        trace!("Status = {:?} for {}", res.status(), res.url());
        let s = res.status();
        let text = res.text().await?;
        handle_api_errors(&text, s)?;

        serde_json::from_str(&text).map_err(|e| {
            warn!("{}, {:?}", text, e);
            Error::SerdeError(e)
        })
    }

    pub async fn request_text(&self, request: http::Request<Vec<u8>>) -> Result<String> {
        let res: reqwest::Response = self.send(request).await?;
        trace!("Status = {:?} for {}", res.status(), res.url());
        let s = res.status();
        let text = res.text().await?;
        handle_api_errors(&text, s)?;

        Ok(text)
    }

    pub async fn request_text_stream(
        &self,
        request: http::Request<Vec<u8>>,
    ) -> Result<impl Stream<Item = Result<Bytes>>> {
        let res: reqwest::Response = self.send(request).await?;
        trace!("Status = {:?} for {}", res.status(), res.url());

        Ok(res.bytes_stream().map_err(Error::ReqwestError))
    }

    pub async fn request_status<T>(&self, request: http::Request<Vec<u8>>) -> Result<Either<T, Status>>
    where
        T: DeserializeOwned,
    {
        let res: reqwest::Response = self.send(request).await?;
        trace!("Status = {:?} for {}", res.status(), res.url());
        let s = res.status();
        let text = res.text().await?;
        handle_api_errors(&text, s)?;

        // It needs to be JSON:
        let v: Value = serde_json::from_str(&text)?;
        if v["kind"] == "Status" {
            trace!("Status from {}", text);
            Ok(Right(serde_json::from_str::<Status>(&text).map_err(|e| {
                warn!("{}, {:?}", text, e);
                Error::SerdeError(e)
            })?))
        } else {
            Ok(Left(serde_json::from_str::<T>(&text).map_err(|e| {
                warn!("{}, {:?}", text, e);
                Error::SerdeError(e)
            })?))
        }
    }

    pub async fn request_events<T>(
        &self,
        request: http::Request<Vec<u8>>,
    ) -> Result<impl TryStream<Item = Result<T>>>
    where
        T: DeserializeOwned,
    {
        let res: reqwest::Response = self.send(request).await?;
        trace!("Streaming from {} -> {}", res.url(), res.status().as_str());
        trace!("headers: {:?}", res.headers());

        // Now unfold the chunked responses into a Stream
        // We first construct a Stream of Vec<Result<T>> as we potentially might need to
        // yield multiple objects per loop, then we flatten it to the Stream<Result<T>> as expected.
        // Any reqwest errors will terminate this stream early.
        let stream = futures::stream::try_unfold((res, Vec::new()), |(mut resp, _buff)| {
            async {
                let mut buff = _buff; // can be avoided, see #145
                loop {
                    trace!("Await chunk");
                    match resp.chunk().await {
                        Ok(Some(chunk)) => {
                            trace!("Some chunk of len {}", chunk.len());
                            buff.extend_from_slice(&chunk);

                            // If we've encountered a newline, see if we have any items to yield
                            if chunk.contains(&b'\n') {
                                let mut new_buff = Vec::new();
                                let mut items = Vec::new();

                                // Split on newlines
                                for line in buff.split(|x| x == &b'\n') {
                                    new_buff.extend_from_slice(&line);

                                    match serde_json::from_slice(&new_buff) {
                                        Ok(val) => {
                                            // on success clear our buffer
                                            new_buff.clear();
                                            items.push(Ok(val));
                                        }
                                        Err(e) => {
                                            // If this is not an eof error it's a parse error
                                            // so log it and store it
                                            // Otherwise we don't do anything as we've already
                                            // added in the current partial line to our buffer for
                                            // use in the next loop
                                            if !e.is_eof() {
                                                // Check if it's a general API error response
                                                let e = match serde_json::from_slice(&new_buff) {
                                                    Ok(e) => Error::Api(e),
                                                    _ => {
                                                        let line = String::from_utf8_lossy(line);
                                                        warn!("Failed to parse: {}", line);
                                                        Error::SerdeError(e)
                                                    }
                                                };

                                                // Clear the buffer as this was a valid object
                                                new_buff.clear();
                                                items.push(Err(e));
                                            }
                                        }
                                    }
                                }

                                // Now return our items and loop
                                return Ok(Some((items, (resp, new_buff))));
                            }
                        }
                        Ok(None) => {
                            trace!("None chunk");
                            return Ok(None);
                        }
                        Err(e) => {
                            if e.is_timeout() {
                                warn!("timeout in poll: {}", e); // our client timeout
                                return Ok(None);
                            }
                            let inner = e.to_string();
                            if inner.contains("unexpected EOF during chunk") {
                                // ^ catches reqwest::Error from hyper::Error
                                // where the inner.kind == UnexpectedEof
                                // and the inner.error == "unexpected EOF during chunk size line"
                                warn!("eof in poll: {}", e);
                                return Ok(None);
                            } else {
                                // There might be other errors worth ignoring here
                                // For now, if they happen, we hard error up
                                // This causes a full re-list for Reflector
                                error!("err poll: {:?} - {}", e, inner);
                                return Err(Error::ReqwestError(e));
                            }
                        }
                    }
                }
            }
        });

        Ok(stream.map_ok(futures::stream::iter).try_flatten())
    }
}

/// Kubernetes returned error handling
///
/// Either kube returned an explicit ApiError struct,
/// or it someohow returned something we couldn't parse as one.
///
/// In either case, present an ApiError upstream.
/// The latter is probably a bug if encountered.
fn handle_api_errors(text: &str, s: StatusCode) -> Result<()> {
    if s.is_client_error() || s.is_server_error() {
        // Print better debug when things do fail
        // trace!("Parsing error: {}", text);
        if let Ok(errdata) = serde_json::from_str::<ErrorResponse>(text) {
            debug!("Unsuccessful: {:?}", errdata);
            Err(Error::Api(errdata))
        } else {
            warn!("Unsuccessful data error parse: {}", text);
            // Propagate errors properly via reqwest
            let ae = ErrorResponse {
                status: s.to_string(),
                code: s.as_u16(),
                message: format!("{:?}", text),
                reason: "Failed to parse error data".into(),
            };
            debug!("Unsuccessful: {:?} (reconstruct)", ae);
            Err(Error::Api(ae))
        }
    } else {
        Ok(())
    }
}

impl std::convert::From<crate::config::ClientConfig> for reqwest::ClientBuilder {
    fn from(config: crate::config::ClientConfig) -> Self {
        let mut builder = Self::new();

        if let Some(c) = config.root_cert {
            builder = builder.add_root_certificate(c);
        }
        builder = builder.default_headers(config.headers);
        if let Some(to) = config.timeout {
            builder = builder.timeout(to);
        }

        builder = builder.danger_accept_invalid_certs(config.accept_invalid_certs);

        if let Some(i) = config.identity {
            builder = builder.identity(i)
        }
        builder
    }
}

#[cfg(test)]
mod test {
    use super::Status;

    // ensure our status schema is sensible
    #[test]
    fn delete_deserialize_test() {
        let statusresp = r#"{"kind":"Status","apiVersion":"v1","metadata":{},"status":"Success","details":{"name":"some-app","group":"clux.dev","kind":"foos","uid":"1234-some-uid"}}"#;
        let s: Status = serde_json::from_str::<Status>(statusresp).unwrap();
        assert_eq!(s.details.unwrap().name, "some-app");

        let statusnoname = r#"{"kind":"Status","apiVersion":"v1","metadata":{},"status":"Success","details":{"group":"clux.dev","kind":"foos","uid":"1234-some-uid"}}"#;
        let s2: Status = serde_json::from_str::<Status>(statusnoname).unwrap();
        assert_eq!(s2.details.unwrap().name, ""); // optional probably better..
    }
}
