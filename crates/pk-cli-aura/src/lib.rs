//! Read a Salesforce Experience Cloud site the way its own pages do.
//!
//! An Experience Cloud site (a customer or member portal on Salesforce,
//! often at `*.my.site.com`) fetches its data with Aura actions:
//! `POST <base>/sfsites/aura`, a form of four fields — `message` (the
//! actions, JSON), `aura.context` (the framework build and app, JSON),
//! `aura.pageURI` and `aura.token` (the signed-in user's CSRF token) — and an
//! answer with one entry per action, matched by id. This crate replays those
//! calls with a session the owner signed in to, so a CLI reads the same JSON
//! the site's pages render, never the DOM.
//!
//! The crate is mechanism: the request form ([`protocol`]), the context the
//! signed-in home page carries ([`bootstrap_context`]), what each answer
//! means for the session ([`Site`]), and a [`Client`] over core's
//! [`Transport`](pk_cli_core::transport::Transport) that reads the context
//! once, retries a call once when the framework moved on, keeps rotated
//! cookies ([`Client::refreshed`]) and maps every answer to the family's
//! exit codes. Which Apex classes and methods a site serves, and which of
//! them a CLI may call, are the CLI's.
//!
//! | The site answers | It means | Exit |
//! |---|---|---|
//! | an action `SUCCESS` | the call ran | 0, [`Answer::take`] |
//! | a `COOSE` warning and a new `fwuid` | the app was redeployed; the actions still ran | 0; one retry only if an action has no result |
//! | no entry for an action | the method is unknown (renamed or removed) | 5 |
//! | an action `ERROR` "You do not have access to the Apex class" | no live session: the call ran as the guest user | 3 |
//! | `*/{"event":{"descriptor":"markup://aura:invalidSession"…}}/*ERROR*/` | no token, or the wrong one | 3 |
//! | a redirect, a 401, or the sign-in page under any status | the session is gone | 3 |
//! | a 429, a 5xx, or a 403 with another HTML page | an edge block, a rate limit or an outage | 5, with its `Retry-After` |
//! | any other HTML | not the app | 5 |
//!
//! A lost session is never only a 401: most of those answers are a `200`.
//!
//! The session itself — the cookie header with `sid`, and the token from
//! the app's `localStorage` (`$AuraClientService.token$<app>`) — comes from
//! a sign-in window (`pk-cli-browser`) and lives in the CLI's keychain item;
//! the client only carries it, and hands back rotated cookies for the CLI to
//! store.
//!
//! ```no_run
//! use pk_cli_aura::{Action, Client, Site};
//! use pk_cli_http::BlockingTransport;
//! use pk_cli_secrets::Secret;
//! use serde_json::json;
//!
//! # fn main() -> Result<(), pk_cli_core::CliError> {
//! # let (cookie_header, token) = (Secret::new("sid=…"), Secret::new("…"));
//! let site = Site::new("example.my.site.com")
//!     .with_name("Example")
//!     .with_login_hint("ex auth login --browser")
//!     .with_user_agent("ex/0.1.0 (unofficial; read-only)");
//! let mut client = Client::new(BlockingTransport::new()?, site, cookie_header, token);
//! let actions = [Action::apex_execute("", "ExampleController", "getThings", json!({}), true)];
//! let answer = pk_cli_core::block_on(client.call(&actions))?;
//! let things = answer
//!     .take_apex(0, "ExampleController.getThings")
//!     .map_err(|e| e.into_cli(client.site()))?;
//! if let Some(cookies) = client.refreshed() {
//!     // store the rotated cookie header back in the keychain item
//! #   let _ = cookies;
//! }
//! # let _ = things;
//! # Ok(())
//! # }
//! ```
//!
//! The crate does not build for WebAssembly: the session is a
//! `pk_cli_secrets::Secret`, and that crate does not.

mod client;
pub mod cookies;
pub mod protocol;
mod site;

pub use client::Client;
pub use cookies::merge_set_cookies;
pub use protocol::{
    bootstrap_context, is_sign_in_page, json_value, parse, Action, Answer, AuraError, Context,
};
pub use site::Site;
