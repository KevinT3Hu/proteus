//! The plugin's own entries in the desktop keyring (Secret Service, as
//! GNOME Keyring and KWallet provide it). Items carry the attributes
//! `application = proteus`, `proteus-plugin = ID` and `key = KEY`, so a
//! plugin can only find its own.

use std::collections::HashMap;

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

const DEST: &str = "org.freedesktop.secrets";
const PATH: &str = "/org/freedesktop/secrets";
const SERVICE: &str = "org.freedesktop.Secret.Service";
const COLLECTION: &str = "/org/freedesktop/secrets/aliases/default";

/// (session, parameters, value, content type)
type Secret = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);

pub struct Keyring {
    conn: Connection,
    session: OwnedObjectPath,
}

fn err(e: zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(name, msg, _) if name.as_str() == "org.freedesktop.DBus.Error.ServiceUnknown" => {
            format!("no keyring (Secret Service) is running: {}", msg.unwrap_or_default())
        }
        e => format!("keyring: {e}"),
    }
}

impl Keyring {
    pub fn open() -> Result<Keyring, String> {
        let conn = Connection::session().map_err(err)?;
        let svc = Proxy::new(&conn, DEST, PATH, SERVICE).map_err(err)?;
        let (_, session): (OwnedValue, OwnedObjectPath) =
            svc.call("OpenSession", &("plain", Value::from(""))).map_err(err)?;
        Ok(Keyring { conn, session })
    }

    fn service(&self) -> Result<Proxy<'_>, String> {
        Proxy::new(&self.conn, DEST, PATH, SERVICE).map_err(err)
    }

    /// Wait for the user to answer a prompt (unlocking the keyring).
    fn prompt(&self, path: &ObjectPath<'_>) -> Result<(), String> {
        if path.as_str() == "/" {
            return Ok(());
        }
        let p = Proxy::new(&self.conn, DEST, path.to_owned(), "org.freedesktop.Secret.Prompt").map_err(err)?;
        let mut done = p.receive_signal("Completed").map_err(err)?;
        p.call::<_, _, ()>("Prompt", &("",)).map_err(err)?;
        let msg = done.next().ok_or("keyring: the prompt went away")?;
        let (dismissed, _): (bool, OwnedValue) = msg.body().deserialize().map_err(|e| format!("keyring: {e}"))?;
        if dismissed { Err("keyring: the user did not unlock it".into()) } else { Ok(()) }
    }

    fn unlock(&self, objects: &[OwnedObjectPath]) -> Result<(), String> {
        if objects.is_empty() {
            return Ok(());
        }
        let (_, prompt): (Vec<OwnedObjectPath>, OwnedObjectPath) =
            self.service()?.call("Unlock", &(objects,)).map_err(err)?;
        self.prompt(&prompt)
    }

    fn find(&self, plugin: &str, key: &str) -> Result<Vec<OwnedObjectPath>, String> {
        let attrs = attributes(plugin, key);
        let (mut unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) =
            self.service()?.call("SearchItems", &(attrs,)).map_err(err)?;
        self.unlock(&locked)?;
        unlocked.extend(locked);
        Ok(unlocked)
    }

    pub fn get(&self, plugin: &str, key: &str) -> Result<Option<String>, String> {
        let Some(item) = self.find(plugin, key)?.into_iter().next() else { return Ok(None) };
        let p = Proxy::new(&self.conn, DEST, item, "org.freedesktop.Secret.Item").map_err(err)?;
        let (secret,): (Secret,) = p.call("GetSecret", &(&self.session,)).map_err(err)?;
        String::from_utf8(secret.2).map(Some).map_err(|_| "keyring: the secret is not text".into())
    }

    pub fn set(&self, plugin: &str, key: &str, value: &str) -> Result<(), String> {
        let collection = OwnedObjectPath::try_from(COLLECTION).map_err(|e| e.to_string())?;
        self.unlock(std::slice::from_ref(&collection))?;
        let mut props: HashMap<&str, Value> = HashMap::new();
        props.insert("org.freedesktop.Secret.Item.Label", Value::from(format!("Proteus plugin {plugin}: {key}")));
        props.insert("org.freedesktop.Secret.Item.Attributes", Value::from(attributes(plugin, key)));
        let secret = (&self.session, Vec::<u8>::new(), value.as_bytes().to_vec(), "text/plain; charset=utf8");
        let c = Proxy::new(&self.conn, DEST, COLLECTION, "org.freedesktop.Secret.Collection").map_err(err)?;
        let (_, prompt): (OwnedObjectPath, OwnedObjectPath) =
            c.call("CreateItem", &(props, secret, true)).map_err(err)?;
        self.prompt(&prompt)
    }

    pub fn delete(&self, plugin: &str, key: &str) -> Result<(), String> {
        for item in self.find(plugin, key)? {
            let p = Proxy::new(&self.conn, DEST, item, "org.freedesktop.Secret.Item").map_err(err)?;
            let prompt: OwnedObjectPath = p.call("Delete", &()).map_err(err)?;
            self.prompt(&prompt)?;
        }
        Ok(())
    }
}

fn attributes(plugin: &str, key: &str) -> HashMap<String, String> {
    HashMap::from([
        ("application".to_string(), "proteus".to_string()),
        ("proteus-plugin".into(), plugin.into()),
        ("key".into(), key.into()),
    ])
}

/// Keys: 1 to 200 bytes, values up to 64 KiB.
pub fn check(key: &str, value: Option<&str>) -> Result<(), String> {
    if key.is_empty() || key.len() > 200 {
        return Err("secret keys are 1 to 200 bytes".into());
    }
    if value.is_some_and(|v| v.len() > 65536) {
        return Err("secrets are at most 64 KiB".into());
    }
    Ok(())
}
