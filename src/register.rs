//! XEP-0077 In-Band Registration: a new account on the server, from the login screen.
//! Servers ask either for the legacy username/password pair or for a data form, which may carry
//! a picture CAPTCHA (XEP-0158, the picture as XEP-0231 bits of binary).
//!
//! Runs on its own thread like the client; the stream stays open from the form to the answer,
//! since the CAPTCHA belongs to it.
use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

use base64::Engine as _;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures::{SinkExt, StreamExt};
use tokio_xmpp::connect::ServerConnector;
use tokio_xmpp::minidom::Element;
use tokio_xmpp::parsers::{
    data_forms::{DataForm, DataFormType, FieldType},
    iq::Iq,
    jid::Jid,
    ns,
    stanza_error::DefinedCondition,
};
use tokio_xmpp::xmlstream::{Timeouts, XmppStreamElement};
use tokio_xmpp::Stanza;

use crate::connector::Connector;
use crate::t;

/// Something to fill in besides username and password.
#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub var: String,
    pub label: String,
    pub private: bool,
}

/// What the server wants for a new account.
#[derive(Clone, Debug, Default)]
pub struct Form {
    pub instructions: Option<String>,
    pub fields: Vec<Field>,
    /// The CAPTCHA picture; its answer goes into the field `ocr`.
    pub captcha: Option<Vec<u8>>,
    /// The data form to send back, or `None` for the legacy fields.
    data: Option<DataForm>,
    /// Legacy fields to send back besides username and password.
    legacy: Vec<String>,
}

impl Form {
    /// Nothing to ask the user beyond the name and password they typed already.
    pub fn ready(&self) -> bool {
        self.fields.is_empty() && self.captcha.is_none()
    }
}

pub enum Ev {
    Form(Form),
    Done,
    Failed(String),
}

pub struct Submit {
    pub username: String,
    pub password: String,
    /// Answers to `Form::fields`, by `var`.
    pub values: HashMap<String, String>,
}

/// Asks `domain` for its registration form; `Ev::Form` comes back, then the `Submit` sent in
/// answers it with `Ev::Done` or `Ev::Failed`.
pub fn start(domain: String, host: Option<String>, port: u16, insecure: bool) -> (UnboundedSender<Submit>, UnboundedReceiver<Ev>) {
    let (submit_tx, submit_rx) = unbounded();
    let (ev_tx, ev_rx) = unbounded();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let ev = ev_tx.clone();
        let res = rt.block_on(run(domain, Connector { host, port, insecure }, submit_rx, ev_tx));
        let _ = ev.unbounded_send(match res {
            Ok(()) => Ev::Done,
            Err(e) => Ev::Failed(e),
        });
    });
    (submit_tx, ev_rx)
}

async fn run(domain: String, connector: Connector, mut submit: UnboundedReceiver<Submit>, ev: UnboundedSender<Ev>) -> Result<(), String> {
    let server = Jid::from_str(&domain).map_err(|e| e.to_string())?;
    let timeouts = Timeouts { read_timeout: Duration::from_secs(60), response_timeout: Duration::from_secs(30) };
    let (pending, _) = connector.connect(&server, ns::JABBER_CLIENT, timeouts).await.map_err(|e| e.to_string())?;
    let (features, mut stream) = pending.recv_features::<XmppStreamElement>().await.map_err(|e| e.to_string())?;
    if !features.in_band_registration {
        return Err(t!("register.unsupported").into());
    }
    let ask = Iq::Get { from: None, to: Some(server.clone()), id: "reg-form".into(), payload: Element::builder("query", ns::REGISTER).build() };
    stream.send(&XmppStreamElement::Stanza(Stanza::Iq(ask))).await.map_err(|e| e.to_string())?;
    let form = parse(&reply(&mut stream, "reg-form").await?);
    let _ = ev.unbounded_send(Ev::Form(form.clone()));
    let Some(answer) = submit.next().await else { return Err(t!("register.cancelled").into()) };
    let iq = Iq::Set { from: None, to: Some(server.clone()), id: "reg-submit".into(), payload: answer_query(&form, &answer) };
    stream.send(&XmppStreamElement::Stanza(Stanza::Iq(iq))).await.map_err(|e| e.to_string())?;
    reply(&mut stream, "reg-submit").await?;
    let _ = <_ as SinkExt<&XmppStreamElement>>::close(&mut stream).await;
    Ok(())
}

/// The payload of the IQ answer with `id`; an IQ error becomes a message for the user.
async fn reply<S>(stream: &mut S, id: &str) -> Result<Element, String>
where
    S: futures::Stream<Item = Result<XmppStreamElement, tokio_xmpp::xmlstream::ReadError>> + Unpin,
{
    use tokio_xmpp::xmlstream::ReadError;
    loop {
        match stream.next().await {
            Some(Ok(XmppStreamElement::Stanza(Stanza::Iq(Iq::Result { id: got, payload, .. })))) if got == id => {
                return Ok(payload.unwrap_or_else(|| Element::builder("query", ns::REGISTER).build()));
            }
            Some(Ok(XmppStreamElement::Stanza(Stanza::Iq(Iq::Error { id: got, error, .. })))) if got == id => {
                return Err(match error.defined_condition {
                    DefinedCondition::Conflict => t!("register.taken").to_string(),
                    DefinedCondition::NotAcceptable | DefinedCondition::BadRequest => {
                        error.texts.values().next().cloned().unwrap_or_else(|| t!("register.not_acceptable").to_string())
                    }
                    DefinedCondition::NotAllowed | DefinedCondition::Forbidden => t!("register.not_allowed").to_string(),
                    DefinedCondition::ResourceConstraint | DefinedCondition::PolicyViolation => t!("register.too_many").to_string(),
                    DefinedCondition::ServiceUnavailable | DefinedCondition::FeatureNotImplemented => t!("register.unsupported").to_string(),
                    other => error.texts.values().next().cloned().unwrap_or_else(|| format!("{other:?}")),
                });
            }
            Some(Ok(XmppStreamElement::StreamError(e))) => return Err(format!("{e:?}")),
            Some(Ok(_)) | Some(Err(ReadError::SoftTimeout)) => {}
            Some(Err(ReadError::HardError(e))) => return Err(e.to_string()),
            Some(Err(ReadError::ParseError(e))) => return Err(e.to_string()),
            Some(Err(ReadError::StreamFooterReceived)) | None => return Err(t!("err.no_connection").into()),
        }
    }
}

/// What the server's `<query xmlns='jabber:iq:register'>` asks for.
pub fn parse(query: &Element) -> Form {
    let instructions = query.get_child("instructions", ns::REGISTER).map(|i| i.text().trim().to_string()).filter(|s| !s.is_empty());
    // XEP-0231: pictures shipped along, by content id.
    let bits: HashMap<String, Vec<u8>> = query
        .children()
        .filter(|c| c.is("data", "urn:xmpp:bob"))
        .filter_map(|d| Some((d.attr("cid")?.to_string(), base64::engine::general_purpose::STANDARD.decode(d.text().split_whitespace().collect::<String>()).ok()?)))
        .collect();
    if let Some(form) = query.get_child("x", ns::DATA_FORMS).and_then(|x| DataForm::try_from(x.clone()).ok()) {
        let mut captcha = None;
        let mut fields = vec![];
        for f in &form.fields {
            let Some(var) = f.var.clone() else { continue };
            if matches!(f.type_, FieldType::Hidden | FieldType::Fixed) || var == "username" || var == "password" {
                continue;
            }
            if let Some(png) = f.media.iter().flat_map(|m| &m.uris).find_map(|u| u.uri.strip_prefix("cid:").and_then(|cid| bits.get(cid))) {
                captcha = Some(png.clone());
            }
            // Optional extras with a default are sent back as they are.
            if !f.required && !f.media.iter().any(|m| !m.uris.is_empty()) && matches!(f.type_, FieldType::ListSingle | FieldType::Boolean | FieldType::ListMulti) {
                continue;
            }
            let label = f.label.clone().unwrap_or_else(|| var.clone());
            fields.push(Field { private: f.type_ == FieldType::TextPrivate, var, label });
        }
        let instructions = form.instructions.clone().or(instructions);
        return Form { instructions, fields, captcha, data: Some(form), legacy: vec![] };
    }
    // Legacy: every empty child but these is something to fill in (email and the like).
    let legacy: Vec<String> = query
        .children()
        .filter(|c| c.ns() == ns::REGISTER && !matches!(c.name(), "instructions" | "username" | "password" | "registered" | "remove" | "x"))
        .map(|c| c.name().to_string())
        .collect();
    let fields = legacy.iter().map(|n| Field { var: n.clone(), label: n.clone(), private: false }).collect();
    Form { instructions, fields, captcha: None, data: None, legacy }
}

/// The `<query>` that answers `form`.
pub fn answer_query(form: &Form, a: &Submit) -> Element {
    let value = |var: &str| match var {
        "username" => Some(a.username.clone()),
        "password" => Some(a.password.clone()),
        v => a.values.get(v).cloned(),
    };
    let mut q = Element::builder("query", ns::REGISTER);
    match &form.data {
        Some(data) => {
            let mut submit = data.clone();
            submit.type_ = DataFormType::Submit;
            submit.title = None;
            submit.instructions = None;
            submit.fields.retain(|f| f.type_ != FieldType::Fixed && f.var.is_some());
            for f in &mut submit.fields {
                f.media.clear();
                f.label = None;
                f.desc = None;
                f.options.clear();
                if let Some(v) = f.var.as_deref().and_then(value) {
                    f.values = vec![v];
                }
            }
            q = q.append(Element::from(submit));
        }
        None => {
            q = q.append(Element::builder("username", ns::REGISTER).append(a.username.as_str())).append(Element::builder("password", ns::REGISTER).append(a.password.as_str()));
            for name in &form.legacy {
                q = q.append(Element::builder(name.as_str(), ns::REGISTER).append(value(name).unwrap_or_default()));
            }
        }
    }
    q.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ejabberd with a CAPTCHA: legacy fields, a data form with an `ocr` field pointing at the picture.
    #[test]
    fn captcha_form() {
        let q: Element = "<query xmlns='jabber:iq:register'>\
            <instructions>Use the form</instructions><username/><password/>\
            <x xmlns='jabber:x:data' type='form'>\
              <instructions>Fill the form</instructions>\
              <field var='FORM_TYPE' type='hidden'><value>jabber:iq:register</value></field>\
              <field label='User' var='username' type='text-single'><required/></field>\
              <field label='Password' var='password' type='text-private'><required/></field>\
              <field type='fixed'><value>If you don't see the CAPTCHA image here, visit the web page.</value></field>\
              <field var='captcha-fallback-url' type='text-single'><value>https://x/captcha/123</value></field>\
              <field var='from' type='hidden'><value>x</value></field>\
              <field var='challenge' type='hidden'><value>123</value></field>\
              <field var='sid' type='hidden'><value></value></field>\
              <field label='Enter the text you see' var='ocr'><media xmlns='urn:xmpp:media-element'><uri type='image/png'>cid:sha1+abc@bob.xmpp.org</uri></media><required/></field>\
            </x>\
            <data xmlns='urn:xmpp:bob' cid='sha1+abc@bob.xmpp.org' type='image/png' max-age='0'>iVBORw0K</data>\
            </query>"
            .parse()
            .unwrap();
        let f = parse(&q);
        assert_eq!(f.instructions.as_deref(), Some("Fill the form"));
        assert_eq!(f.captcha.as_deref(), Some(&[0x89, b'P', b'N', b'G', b'\r', b'\n'][..]));
        let vars: Vec<&str> = f.fields.iter().map(|x| x.var.as_str()).collect();
        assert_eq!(vars, ["captcha-fallback-url", "ocr"]);
        assert!(!f.ready());

        let mut values = HashMap::new();
        values.insert("ocr".to_string(), "7x2k".to_string());
        values.insert("captcha-fallback-url".to_string(), "https://x/captcha/123".to_string());
        let a = answer_query(&f, &Submit { username: "dave".into(), password: "pw".into(), values });
        let form = DataForm::try_from(a.get_child("x", ns::DATA_FORMS).unwrap().clone()).unwrap();
        assert_eq!(form.type_, DataFormType::Submit);
        let get = |v: &str| form.fields.iter().find(|f| f.var.as_deref() == Some(v)).map(|f| f.values.join(""));
        assert_eq!(get("username").as_deref(), Some("dave"));
        assert_eq!(get("password").as_deref(), Some("pw"));
        assert_eq!(get("ocr").as_deref(), Some("7x2k"));
        assert_eq!(get("challenge").as_deref(), Some("123"), "hidden fields go back");
        assert_eq!(get("FORM_TYPE").as_deref(), Some("jabber:iq:register"));
    }

    #[test]
    fn legacy_form() {
        let q: Element = "<query xmlns='jabber:iq:register'><instructions>Choose a name</instructions><username/><password/><email/></query>".parse().unwrap();
        let f = parse(&q);
        assert_eq!(f.fields, [Field { var: "email".into(), label: "email".into(), private: false }]);
        let mut values = HashMap::new();
        values.insert("email".to_string(), "d@x".to_string());
        let a = answer_query(&f, &Submit { username: "dave".into(), password: "pw".into(), values });
        assert_eq!(a.get_child("username", ns::REGISTER).unwrap().text(), "dave");
        assert_eq!(a.get_child("email", ns::REGISTER).unwrap().text(), "d@x");
        assert!(parse(&"<query xmlns='jabber:iq:register'><username/><password/></query>".parse().unwrap()).ready());
    }
}
