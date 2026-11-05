//! Full v1 musical observation, not native receiver admission. Positive fields only.
use serde_json::Value;
use std::collections::HashSet;
const SAFE: u64 = 9_007_199_254_740_991;

/// serde_json::Value alone silently accepts duplicate keys. Coordinator envelopes
/// must not select a different interpretation at different validation layers.
pub struct StrictValue(pub Value);
impl<'de> serde::Deserialize<'de> for StrictValue {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = StrictValue;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("JSON with unique object fields")
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<StrictValue, E> {
                Ok(StrictValue(v.into()))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<StrictValue, E> {
                Ok(StrictValue(v.into()))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<StrictValue, E> {
                Ok(StrictValue(v.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<StrictValue, E> {
                serde_json::Number::from_f64(v)
                    .map(|v| StrictValue(Value::Number(v)))
                    .ok_or_else(|| E::custom("invalid number"))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<StrictValue, E> {
                Ok(StrictValue(v.into()))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<StrictValue, E> {
                Ok(StrictValue(v.into()))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<StrictValue, E> {
                Ok(StrictValue(Value::Null))
            }
            fn visit_none<E: serde::de::Error>(self) -> Result<StrictValue, E> {
                Ok(StrictValue(Value::Null))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut a: A,
            ) -> Result<StrictValue, A::Error> {
                let mut v = Vec::new();
                while let Some(StrictValue(item)) = a.next_element()? {
                    v.push(item);
                }
                Ok(StrictValue(Value::Array(v)))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut a: A,
            ) -> Result<StrictValue, A::Error> {
                let mut v = serde_json::Map::new();
                while let Some(k) = a.next_key::<String>()? {
                    if v.contains_key(&k) {
                        return Err(serde::de::Error::custom("duplicate field"));
                    }
                    let StrictValue(item) = a.next_value()?;
                    v.insert(k, item);
                }
                Ok(StrictValue(Value::Object(v)))
            }
        }
        d.deserialize_any(Visitor)
    }
}
fn fields(v: &Value, required: &[&str], optional: &[&str]) -> bool {
    v.as_object().is_some_and(|o| {
        required.iter().all(|k| o.contains_key(*k))
            && o.keys()
                .all(|k| required.contains(&k.as_str()) || optional.contains(&k.as_str()))
    })
}
fn text(v: &Value, max: usize) -> bool {
    v.as_str()
        .is_some_and(|s| s.len() <= max && !s.chars().any(char::is_control))
}
fn id(v: &Value, optional: bool) -> bool {
    text(v, 512)
        && v.as_str()
            .is_some_and(|s| (optional || !s.is_empty()) && s.trim() == s)
}
fn number(v: &Value) -> bool {
    v.as_f64()
        .is_some_and(|n| n.is_finite() && (0.0..=604800.0).contains(&n))
}
fn integer(v: &Value, min: u64) -> bool {
    v.as_u64().is_some_and(|n| (min..=SAFE).contains(&n))
}
fn context(v: &Value) -> bool {
    if v.is_null() {
        return true;
    }
    fields(v, &["kind", "id", "name"], &["revision", "fence"])
        && matches!(
            v["kind"].as_str(),
            Some("album" | "artist" | "playlist" | "likes" | "derived")
        )
        && id(&v["id"], false)
        && text(&v["name"], 512)
        && v.get("revision").is_none_or(|v| integer(v, 0))
        && v.get("fence").is_none_or(|v| text(v, 4096))
        && (v["kind"] != "derived"
            || (integer(&v["revision"], 1)
                && v["fence"].as_str().is_some_and(|s| {
                    !s.is_empty() && s.bytes().all(|c| (0x21..=0x7e).contains(&c))
                })))
}
pub fn valid_track(v: &Value, prepared: bool) -> bool {
    fields(
        v,
        &["id", "name", "artists", "albumName", "albumId", "duration"],
        &[
            "key",
            "order",
            "entryId",
            "targetId",
            "occurrenceId",
            "playbackContext",
        ],
    ) && id(&v["id"], false)
        && text(&v["name"], 512)
        && text(&v["albumName"], 512)
        && id(&v["albumId"], true)
        && number(&v["duration"])
        && (!prepared || (integer(&v["key"], 1) && integer(&v["order"], 0)))
        && ["entryId", "targetId"]
            .iter()
            .all(|k| v.get(*k).is_none_or(|v| id(v, true)))
        && v.get("occurrenceId").is_none_or(|v| text(v, 4096))
        && v.get("playbackContext").is_none_or(context)
        && v["artists"].as_array().is_some_and(|a| {
            a.len() <= 64
                && a.iter().all(|a| {
                    fields(a, &["name"], &["id"])
                        && text(&a["name"], 512)
                        && a.get("id").is_none_or(|v| id(v, true))
                })
        })
}
pub fn valid_state(s: &Value) -> bool {
    if !fields(
        s,
        &[
            "version",
            "queue",
            "position",
            "context",
            "shuffle",
            "repeat",
            "elapsed",
            "duration",
            "playing",
            "waiting",
            "error",
            "truncated",
            "notice",
            "occurrence",
        ],
        &["loading"],
    ) || s["version"] != 1
        || !context(&s["context"])
        || !["shuffle", "playing", "waiting", "truncated"]
            .iter()
            .all(|k| s[*k].is_boolean())
        || s.get("loading").is_some_and(|v| !v.is_boolean())
        || !matches!(s["repeat"].as_str(), Some("off" | "all" | "one"))
        || !matches!(
            s["error"].as_str(),
            Some("" | "blocked" | "media" | "queue")
        )
        || !matches!(
            s["notice"].as_str(),
            Some("" | "PLAYBACK_LIMIT" | "CLICKED_OCCURRENCE_ONLY")
        )
        || !number(&s["elapsed"])
        || !number(&s["duration"])
    {
        return false;
    }
    let Some(q) = s["queue"].as_array() else {
        return false;
    };
    let Some(pos) = s["position"].as_i64() else {
        return false;
    };
    if q.len() > 1000
        || (q.is_empty() && pos != -1)
        || (!q.is_empty() && (pos < 0 || pos as usize >= q.len()))
    {
        return false;
    }
    let (mut keys, mut orders) = (HashSet::new(), HashSet::new());
    if !q.iter().all(|t| {
        valid_track(t, true)
            && keys.insert(t["key"].as_u64().unwrap())
            && orders.insert(t["order"].as_u64().unwrap())
    }) {
        return false;
    }
    let o = &s["occurrence"];
    o.is_null()
        || (pos >= 0
            && fields(
                o,
                &["key", "trackId", "eventId", "progress", "reported"],
                &[],
            )
            && o["key"] == q[pos as usize]["key"]
            && o["trackId"] == q[pos as usize]["id"]
            && o["eventId"].as_str().is_some_and(|s| {
                !s.is_empty() && s.len() <= 128 && s.bytes().all(|c| (0x21..=0x7e).contains(&c))
            })
            && number(&o["progress"])
            && o["reported"].is_boolean())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    pub fn full() -> Value {
        let mut s = crate::player::empty_snapshot();
        s["queue"] = json!([
            {"id":"same","name":"First occurrence","artists":[{"name":"Manual artist"}],"albumName":"Album","albumId":"a","duration":123,"key":1,"order":0,"entryId":"first"},
            {"id":"same","name":"Second occurrence","artists":[{"id":"artist","name":"Artist"}],"albumName":"Album","albumId":"a","duration":123,"key":2,"order":1,"entryId":"second","occurrenceId":"opaque","playbackContext":{"kind":"derived","id":"derived","name":"Derived","revision":1,"fence":"opaque-fence"}}
        ]);
        s["position"] = json!(1);
        s["context"] = json!({"kind":"playlist","id":"list","name":"Playlist"});
        s["shuffle"] = json!(true);
        s["repeat"] = json!("all");
        s["occurrence"] =
            json!({"key":2,"trackId":"same","eventId":"event","progress":10,"reported":true});
        s
    }
    #[test]
    fn full_duplicates_context_occurrence_are_observation_not_receiver_admission() {
        let s = full();
        assert!(valid_state(&s));
        assert!(!crate::player::valid_snapshot(&s));
        let mut bad = s.clone();
        bad["queue"][1]["key"] = json!(1);
        assert!(!valid_state(&bad));
        let mut bad = s.clone();
        bad["occurrence"]["key"] = json!(1);
        assert!(!valid_state(&bad));
        let mut bad = s.clone();
        bad["queue"][0]["selectionToken"] = json!("never retain");
        assert!(!valid_state(&bad));
        let mut bad = s;
        bad["queue"][0]["path"] = json!("never retain");
        assert!(!valid_state(&bad));
    }
    #[test]
    fn thousand_entry_bound_and_name_only_artist() {
        let mut s = full();
        s["occurrence"] = Value::Null;
        let t = s["queue"][0].clone();
        s["queue"] = Value::Array(
            (0..1000)
                .map(|i| {
                    let mut t = t.clone();
                    t["key"] = json!(i + 1);
                    t["order"] = json!(i);
                    t
                })
                .collect(),
        );
        assert!(valid_state(&s));
        s["queue"].as_array_mut().unwrap().push(t);
        assert!(!valid_state(&s));
    }
}
