//! Chat API implementation.
use std::io;
use std::str::{self, FromStr};
use std::sync::Arc;

use futures::{Future, BoxFuture};
use tokio_core::channel::Sender;
use tokio_core::reactor::Handle;
use minihttp::Request;
use minihttp::enums::{Status, Method};
use minihttp::client::{HttpClient, Response as ClientResponse};
use rustc_serialize::json::{self, Json};

use intern::SessionId;
use websocket::Base64;
use super::{Cid, ProcessorPool};
use super::{serialize_cid};
use super::router::MessageRouter;
use super::processor::{Action, ConnectionMessage};
use super::message::{Meta, Kwargs, Message, MessageError};

pub struct ChatAPI {
    // shared between connections
    client: HttpClient,         // gets cloned for each request;
    router: MessageRouter,      // singleton per handler endpoint;
    proc_pool: ProcessorPool,   // singleton per handler endpoint;
}

pub struct SessionAPI {
    api: ChatAPI,

    session_id: SessionId,
    auth_token: String,
    conn_id: Cid,   // connection id should not be used here;
    channel: Sender<ConnectionMessage>,
}

impl ChatAPI {
    pub fn new(http_client: HttpClient, router: MessageRouter,
        proc_pool: ProcessorPool)
        -> ChatAPI
    {
        ChatAPI {
            client: http_client,
            router: router,
            proc_pool: proc_pool,
        }
    }

    /// Issue Auth call to backend.
    ///
    /// Send Auth message to proper backend
    /// returninng Hello/Error message.
    pub fn authorize_connection(&self, req: &Request, conn_id: Cid,
        channel: Sender<ConnectionMessage>)
        -> BoxFuture<ClientResponse, io::Error>
        // TODO: convert ClientResponse to Json or ConnectionMessage;
        //  Error to Status?
    {
        let http_cookies = req.headers.iter()
            .filter(|&&(ref k, _)| k == "Cookie")
            .map(|&(_, ref v)| v.clone())
            .collect::<String>();
        let http_auth = req.headers.iter()
            .find(|&&(ref k, _)| k == "Authorization")
            .map(|&(_, ref v)| v.clone())
            .unwrap_or("".to_string());
        let mut meta = Meta::new();
        let mut data = Kwargs::new();

        meta.insert("connection_id".to_string(),
            Json::String(serialize_cid(&conn_id)));
        // TODO: parse cookie string to hashmap;
        data.insert("http_cookie".into(),
            Json::String(http_cookies));
        data.insert("http_authorization".into(),
            Json::String(http_auth));

        self.proc_pool.send(Action::NewConnection {
            conn_id: conn_id,
            channel: channel,
        });

        let payload = Message::Auth(data).encode_with(&meta);
        let mut req = self.client.clone();
        req.request(Method::Post,
            self.router.get_auth_url().as_str());
        req.add_header("Content-Type".into(), "application/json");
        req.add_length(payload.as_bytes().len() as u64);
        req.done_headers();
        req.write_body(payload.as_bytes());
        req.done()
    }

    fn post(&self, method: &str, auth: &str, payload: &[u8])
        -> BoxFuture<ClientResponse, io::Error>
    {
        let url = self.router.resolve(method);
        let mut req = self.client.clone();
        req.request(Method::Post, url.as_str());
        req.add_header("Content-Type".into(), "application/json");
        req.add_header("Authorization".into(), auth);
        req.add_length(payload.len() as u64);
        req.done_headers();
        req.write_body(payload);
        req.done()
        // Result value must be either parsed message or parsed error;
    }

    /// Make instance of Session API (api bound to cid/ssid/tx-channel)
    /// and associate this session with ws connection
    pub fn session_api(self, session_id: SessionId, conn_id: Cid,
        userinfo: Json, channel: Sender<ConnectionMessage>)
        -> SessionAPI
    {
        // XXX: symbol cant be encoded in simple way
        fn encode(s: &SessionId) -> String {
            let auth = format!("{{\"user_id\":{}}}",
                json::encode(&s[..].to_string()).unwrap());
            format!("Tangle {}", Base64(auth.as_bytes()))
        }

        self.proc_pool.send(Action::Associate {
            conn_id: conn_id,
            session_id: session_id.clone(),
            metadata: Arc::new(userinfo),
        });

        SessionAPI {
            api: self,
            auth_token: encode(&session_id),
            session_id: session_id,
            conn_id: conn_id,
            channel: channel,
        }
    }
}

// only difference from ChatAPI -> Bound to concrete SessionId
impl SessionAPI {
    /// Send disconnect to processor.
    pub fn disconnect(&self) {
        self.api.proc_pool.send(Action::Disconnect { conn_id: self.conn_id });
    }

    pub fn method_call(&self, mut meta: Meta, message: Message, handle: &Handle)
    {
        let tx = self.channel.clone();
        let payload = message.encode_with(&meta);
        let call = self.api.post(message.method(),
            self.auth_token.as_str(), payload.as_bytes());
        handle.spawn(call
            .map_err(|e| info!("Http Error: {:?}", e))
            .and_then(move |resp| {
                let result = parse_response(resp)
                    .map(|data| Message::Result(data))
                    .unwrap_or_else(|e| {
                        let e = Message::Error(e);
                        e.update_meta(&mut meta);
                        e
                    })
                    .encode_with(&meta);
                // XXX: use proper ConnectionMessage
                tx.send(ConnectionMessage::Raw(result))
                .map_err(|e| info!("Remote send error: {:?}", e))
            })
        );
    }
}


/// Parse backend response.
fn parse_response(response: ClientResponse) -> Result<Json, MessageError>
{
    // TODO: check content-type
    let payload = match response.body {
        Some(ref data) => {
            str::from_utf8(&data[..])
            .map_err(|e| MessageError::from(e))
            .and_then(
                |s| Json::from_str(s).map_err(|e| MessageError::from(e))
            )?
        }
        None => Json::Null,
    };
    match (response.status, payload) {
        (Status::Ok, payload) => Ok(payload),
        (s, Json::Null) => Err(MessageError::HttpError(s, None)),
        (s, payload) => Err(MessageError::HttpError(s, Some(payload))),
    }
}

/// Parse userinfo received on Auth call;
pub fn parse_userinfo(response: ClientResponse) -> Message {
    use super::message::ValidationError::*;
    use super::message::MessageError::*;
    match parse_response(response) {
        Ok(Json::Object(data)) => {
            let sess_id = match data.get("user_id".into()) {
                Some(&Json::String(ref s)) => {
                    SessionId::from_str(s.as_str()).unwrap()  // XXX
                }
                _ => return Message::Error(ValidationError(InvalidUserId)),
            };
            Message::Hello(sess_id, Json::Object(data))
        }
        Ok(_) => Message::Error(ValidationError(ObjectExpected)),
        Err(err) => Message::Error(err),
    }
}
