//! Linux Secret Service backend. Never opens a prompt, unlocks a collection, or
//! falls back to a file/helper. A plain Secret Service session is confined to a
//! Unix session-bus socket and a single unique service owner; no TCP transport.

use std::{collections::HashMap, time::Duration};

use serde::Serialize;
use zbus::{
    Address, Connection,
    address::Transport,
    connection::Builder,
    names::OwnedUniqueName,
    zvariant::{DynamicDeserialize, DynamicType, OwnedObjectPath, OwnedValue, Signature, Value},
};

use super::{Keyring, MAX_SECRET, Result, SecretService, Slot, StoreError};

const SERVICE: &str = "org.freedesktop.secrets";
const SERVICE_PATH: &str = "/org/freedesktop/secrets";
const SERVICE_IFACE: &str = "org.freedesktop.Secret.Service";
const COLLECTION_IFACE: &str = "org.freedesktop.Secret.Collection";
const ITEM_IFACE: &str = "org.freedesktop.Secret.Item";
const TIMEOUT: Duration = Duration::from_secs(4);
const UNAVAILABLE: StoreError = StoreError::KeyringUnavailable;
const CONTENT_TYPE: &str = "text/plain; charset=utf8";
const REPLY_MAX: usize = MAX_SECRET + 4096;

impl Keyring for SecretService {
    async fn lookup(&self, slot: Slot) -> Result<Option<Vec<u8>>> {
        within(async {
            let bus = Service::connect().await?;
            let Some(item) = bus.find(slot).await? else {
                return Ok(None);
            };
            let session = bus.session().await?;
            // Providers can normalize the MIME type (e.g. text/plain or
            // application/octet-stream). The pinned service/session, slot and
            // later bounded JSON schema check establish validity, not MIME.
            let (returned, params, blob, _content): (OwnedObjectPath, Vec<u8>, Vec<u8>, String) =
                bus.call(item.as_str(), ITEM_IFACE, "GetSecret", &(&session,))
                    .await?;
            if returned != session || !params.is_empty() {
                return Err(StoreError::InvalidCredential);
            }
            Ok(Some(blob))
        })
        .await
    }

    async fn store(&self, slot: Slot, blob: &[u8]) -> Result<()> {
        within(async {
            let bus = Service::connect().await?;
            let collection: OwnedObjectPath = bus
                .call(SERVICE_PATH, SERVICE_IFACE, "ReadAlias", &("default",))
                .await?;
            if collection.as_str() == "/" {
                return Err(UNAVAILABLE);
            }
            let locked: OwnedValue = bus
                .call(
                    collection.as_str(),
                    "org.freedesktop.DBus.Properties",
                    "Get",
                    &(COLLECTION_IFACE, "Locked"),
                )
                .await?;
            if bool::try_from(locked).map_err(|_| UNAVAILABLE)? {
                return Err(UNAVAILABLE);
            }
            let session = bus.session().await?;
            let properties = HashMap::from([
                (
                    "org.freedesktop.Secret.Item.Label",
                    Value::from("Spofify Desktop remembered login"),
                ),
                (
                    "org.freedesktop.Secret.Item.Attributes",
                    Value::from(attributes(slot)),
                ),
            ]);
            let secret = (&session, &[] as &[u8], blob, CONTENT_TYPE);
            let (item, prompt): (OwnedObjectPath, OwnedObjectPath) = bus
                .call(
                    collection.as_str(),
                    COLLECTION_IFACE,
                    "CreateItem",
                    &(properties, secret, false),
                )
                .await?;
            if item.as_str() == "/" || prompt.as_str() != "/" {
                return Err(UNAVAILABLE);
            }
            Ok(())
        })
        .await
    }

    async fn clear(&self, slot: Slot) -> Result<()> {
        within(async {
            let bus = Service::connect().await?;
            let Some(item) = bus.find(slot).await? else {
                return Ok(());
            };
            let prompt: OwnedObjectPath =
                bus.call(item.as_str(), ITEM_IFACE, "Delete", &()).await?;
            if prompt.as_str() != "/" || bus.find(slot).await?.is_some() {
                return Err(UNAVAILABLE);
            }
            Ok(())
        })
        .await
    }
}

async fn within<T>(future: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(TIMEOUT, future)
        .await
        .map_err(|_| UNAVAILABLE)?
}

fn attributes(slot: Slot) -> HashMap<&'static str, String> {
    HashMap::from([
        ("application", "spofify-desktop-remembered-login-v1".into()),
        ("slot", slot.into()),
    ])
}

fn empty_plain_output(output: &Value<'_>) -> bool {
    match output {
        Value::Str(text) => text.as_str().is_empty(),
        // KWallet returns an empty byte array rather than an empty string.
        Value::Array(bytes) => bytes.element_signature() == &Signature::U8 && bytes.is_empty(),
        _ => false,
    }
}

struct Service {
    bus: Connection,
    owner: OwnedUniqueName,
}

impl Service {
    async fn connect() -> Result<Self> {
        let addr = Address::session().map_err(|_| UNAVAILABLE)?;
        if !matches!(addr.transport(), Transport::Unix(_)) {
            return Err(UNAVAILABLE);
        }
        let bus = Builder::address(addr)
            .map_err(|_| UNAVAILABLE)?
            .build()
            .await
            .map_err(|_| UNAVAILABLE)?;
        let dbus = zbus::fdo::DBusProxy::new(&bus)
            .await
            .map_err(|_| UNAVAILABLE)?;
        // Activate only the standard Secret Service name, then pin the unique
        // owner for this operation (including the session and secret transfer).
        let name = zbus::names::WellKnownName::try_from(SERVICE).map_err(|_| UNAVAILABLE)?;
        let _ = dbus.start_service_by_name(name.clone(), 0).await;
        let owner = dbus
            .get_name_owner(name.into())
            .await
            .map_err(|_| UNAVAILABLE)?;
        Ok(Self { bus, owner })
    }

    async fn session(&self) -> Result<OwnedObjectPath> {
        let (output, session): (OwnedValue, OwnedObjectPath) = self
            .call(
                SERVICE_PATH,
                SERVICE_IFACE,
                "OpenSession",
                &("plain", Value::from("")),
            )
            .await?;
        if session.as_str() == "/" || !empty_plain_output(&output) {
            return Err(UNAVAILABLE);
        }
        // The per-operation bus connection owns the session; disconnect closes it.
        Ok(session)
    }

    async fn find(&self, slot: Slot) -> Result<Option<OwnedObjectPath>> {
        let (mut unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) = self
            .call(
                SERVICE_PATH,
                SERVICE_IFACE,
                "SearchItems",
                &(attributes(slot),),
            )
            .await?;
        if !locked.is_empty() || unlocked.len() > 1 || unlocked.iter().any(|p| p.as_str() == "/") {
            return Err(UNAVAILABLE);
        }
        Ok(unlocked.pop())
    }

    async fn call<B, R>(&self, path: &str, interface: &str, method: &str, body: &B) -> Result<R>
    where
        B: Serialize + DynamicType,
        R: for<'a> DynamicDeserialize<'a>,
    {
        let reply = self
            .bus
            .call_method(
                Some(self.owner.as_str()),
                path,
                Some(interface),
                method,
                body,
            )
            .await
            .map_err(|_| UNAVAILABLE)?;
        let body = reply.body();
        if body.data().len() > REPLY_MAX {
            return Err(UNAVAILABLE);
        }
        body.deserialize().map_err(|_| UNAVAILABLE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_session_accepts_only_empty_string_or_empty_byte_array() {
        assert!(empty_plain_output(&Value::from("")));
        assert!(empty_plain_output(&Value::from(Vec::<u8>::new())));
        assert!(!empty_plain_output(&Value::from("not empty")));
        assert!(!empty_plain_output(&Value::from(vec![0u8])));
        assert!(!empty_plain_output(&Value::from(Vec::<u32>::new())));
        assert!(!empty_plain_output(&Value::from(false)));
    }
}
