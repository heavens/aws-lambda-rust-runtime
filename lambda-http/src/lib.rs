#![warn(missing_docs, rust_2018_idioms)]
//#![deny(warnings)]
//! Enriches the `lambda` crate with [`http`](https://github.com/hyperium/http)
//! types targeting AWS [ALB](https://docs.aws.amazon.com/elasticloadbalancing/latest/application/introduction.html), [API Gateway](https://docs.aws.amazon.com/apigateway/latest/developerguide/welcome.html) REST and HTTP API lambda integrations.
//!
//! This crate abstracts over all of these trigger events using standard [`http`](https://github.com/hyperium/http) types minimizing the mental overhead
//! of understanding the nuances and variation between trigger details allowing you to focus more on your application while also giving you to the maximum flexibility to
//! transparently use whichever lambda trigger suits your application and cost optimizations best.
//!
//! # Examples
//!
//! ## Hello World
//!
//! The following example is how you would structure your Lambda such that you have a `main` function where you explicitly invoke
//! `lambda_http::run` in combination with the [`service_fn`](fn.service_fn.html) function. This pattern allows you to utilize global initialization
//! of tools such as loggers, to use on warm invokes to the same Lambda function after the first request, helping to reduce the latency of
//! your function's execution path.
//!
//! ```rust,no_run
//! use lambda_http::{service_fn, Error};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Error> {
//!     // initialize dependencies once here for the lifetime of your
//!     // lambda task
//!     lambda_http::run(service_fn(|request| async {
//!         Result::<&str, std::convert::Infallible>::Ok("👋 world!")
//!     })).await?;
//!     Ok(())
//! }
//! ```
//!
//! ## Leveraging trigger provided data
//!
//! You can also access information provided directly from the underlying trigger events, like query string parameters,
//! or Lambda function context, with the [`RequestExt`](trait.RequestExt.html) trait.
//!
//! ```rust,no_run
//! use lambda_http::{service_fn, Error, IntoResponse, Request, RequestExt};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Error> {
//!     lambda_http::run(service_fn(hello)).await?;
//!     Ok(())
//! }
//!
//! async fn hello(
//!     request: Request
//! ) -> Result<impl IntoResponse, std::convert::Infallible> {
//!     let _context = request.lambda_context();
//!
//!     Ok(format!(
//!         "hello {}",
//!         request
//!             .query_string_parameters()
//!             .first("name")
//!             .unwrap_or_else(|| "stranger")
//!     ))
//! }
//! ```

// only externed because maplit doesn't seem to play well with 2018 edition imports
#[cfg(test)]
#[macro_use]
extern crate maplit;

pub use http::{self, Response};
use lambda_runtime::LambdaEvent;
pub use lambda_runtime::{self, service_fn, tower, Context, Error, Service};
use request::RequestFuture;
use response::ResponseFuture;

pub mod ext;
pub mod request;
mod response;
pub use crate::{ext::RequestExt, response::IntoResponse};
use crate::{
    request::{LambdaRequest, RequestOrigin},
    response::LambdaResponse,
};

#[cfg(feature = "alb")]
pub use aws_lambda_events::alb;
#[cfg(any(feature = "apigw_rest", feature = "apigw_http", feature = "apigw_websockets"))]
pub use aws_lambda_events::apigw;

pub use aws_lambda_events::encodings::Body;
use std::{
    future::Future,
    marker::PhantomData,
    pin::Pin,
    task::{Context as TaskContext, Poll},
};

/// Type alias for `http::Request`s with a fixed [`Body`](enum.Body.html) type
pub type Request = http::Request<Body>;

/// Future that will convert an [`IntoResponse`] into an actual [`LambdaResponse`]
///
/// This is used by the `Adapter` wrapper and is completely internal to the `lambda_http::run` function.
#[doc(hidden)]
pub enum TransformResponse<'a, R, E> {
    Request(RequestOrigin, RequestFuture<'a, R, E>),
    Response(RequestOrigin, ResponseFuture),
}

impl<'a, R, E> Future for TransformResponse<'a, R, E>
where
    R: IntoResponse,
{
    type Output = Result<LambdaResponse, E>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        match *self {
            TransformResponse::Request(ref mut origin, ref mut request) => match request.as_mut().poll(cx) {
                Poll::Ready(Ok(resp)) => {
                    *self = TransformResponse::Response(origin.clone(), resp.into_response());
                    self.poll(cx)
                }
                Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
                Poll::Pending => Poll::Pending,
            },
            TransformResponse::Response(ref mut origin, ref mut response) => match response.as_mut().poll(cx) {
                Poll::Ready(resp) => Poll::Ready(Ok(LambdaResponse::from_response(origin, resp))),
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

/// Wraps a `Service<Request>` in a `Service<LambdaEvent<Request>>`
///
/// This is completely internal to the `lambda_http::run` function.
#[doc(hidden)]
pub struct Adapter<'a, R, S> {
    service: S,
    _phantom_data: PhantomData<&'a R>,
}

impl<'a, R, S, E> From<S> for Adapter<'a, R, S>
where
    S: Service<Request, Response = R, Error = E>,
    S::Future: 'a,
    R: IntoResponse,
{
    fn from(service: S) -> Self {
        Adapter {
            service,
            _phantom_data: PhantomData,
        }
    }
}

impl<'a, R, S, E> Service<LambdaEvent<LambdaRequest>> for Adapter<'a, R, S>
where
    S: Service<Request, Response = R, Error = E>,
    S::Future: 'a,
    R: IntoResponse,
{
    type Response = LambdaResponse;
    type Error = E;
    type Future = TransformResponse<'a, R, Self::Error>;

    fn poll_ready(&mut self, cx: &mut core::task::Context<'_>) -> core::task::Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, req: LambdaEvent<LambdaRequest>) -> Self::Future {
        let request_origin = req.payload.request_origin();
        let event: Request = req.payload.into();
        let fut = Box::pin(self.service.call(event.with_lambda_context(req.context)));

        TransformResponse::Request(request_origin, fut)
    }
}

/// Starts the Lambda Rust runtime and begins polling for events on the [Lambda
/// Runtime APIs](https://docs.aws.amazon.com/lambda/latest/dg/runtimes-api.html).
///
/// This takes care of transforming the LambdaEvent into a [`Request`] and then
/// converting the result into a [`LambdaResponse`].
pub async fn run<'a, R, S, E>(handler: S) -> Result<(), Error>
where
    S: Service<Request, Response = R, Error = E>,
    S::Future: 'a,
    R: IntoResponse,
    E: std::fmt::Debug + std::fmt::Display,
{
    lambda_runtime::run(Adapter::from(handler)).await
}
