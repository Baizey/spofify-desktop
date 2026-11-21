//! Listener catalog and indexed search. Declare as a child of `api.rs` with
//! `#[path = "catalog.rs"] pub mod catalog;` so credentials and HTTP stay private.
//!
//! Browser interface on `Api` (`&Session` first):
//! `catalog_detail(kind, id)`, `album_tracks(id, page)`,
//! `artist_albums(id, page)`, `unified_search(query, kind, limit)`,
//! `unified_search_next(cursor)`. `kind` is `CatalogKind`, except search takes
//! `Option<CatalogKind>` (None = all). Personal state uses the account socket.

use std::collections::HashSet;

use reqwest::{StatusCode, header};
use serde::{Deserialize, de::DeserializeOwned};
use url::Url;

use super::{
    Api, ApiError, INVALID_INPUT, INVALID_RESPONSE, Session, decode, valid_id, valid_text,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CatalogKind {
    Artist,
    Album,
    Track,
}

impl CatalogKind {
    pub const fn singular(self) -> &'static str {
        match self {
            Self::Artist => "artist",
            Self::Album => "album",
            Self::Track => "track",
        }
    }

    pub const fn plural(self) -> &'static str {
        match self {
            Self::Artist => "artists",
            Self::Album => "albums",
            Self::Track => "tracks",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageRequest {
    pub offset: u32,
    pub limit: u32,
}

impl Default for PageRequest {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: super::PAGE_SIZE,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ArtistCredit {
    /// Empty for a name-only credit; it is not a navigation target.
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Coverage {
    pub tracks: u64,
    pub in_library: u64,
    pub albums: u64,
}

/// Positive presentation projection only; no filesystem paths or external URLs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogItem {
    pub kind: CatalogKind,
    pub id: String,
    pub name: String,
    pub artists: Vec<ArtistCredit>,
    pub genres: Vec<String>,
    pub album_id: String,
    pub album_name: String,
    pub album_type: String,
    pub release_date: String,
    pub relation: String,
    pub duration_ms: u64,
    pub disc_number: u32,
    pub track_number: u32,
    /// For tracks this indicates published audio, not file/codec validation.
    pub has_audio: bool,
    pub coverage: Coverage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogPage {
    pub items: Vec<CatalogItem>,
    pub total: u64,
    pub offset: u32,
    pub limit: u32,
    pub revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogDetail {
    pub item: CatalogItem,
    pub revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchPage {
    pub items: Vec<CatalogItem>,
    pub result_count: u32,
    pub next_cursor: Option<String>,
    /// Search snapshot metadata, not local-clock authority. Empty only when no request was made.
    pub expires_at: String,
}

impl Api {
    pub async fn catalog_detail(
        &self,
        session: &Session,
        kind: CatalogKind,
        id: &str,
    ) -> Result<CatalogDetail, ApiError> {
        let wire: WireCatalogDetail = self
            .catalog_get(session, Route::Detail(kind, id), &[])
            .await?;
        let item = wire
            .item
            .project(Some(kind), session.role == "admin", false)?;
        if item.id != id || wire.revision > i64::MAX as u64 {
            return Err(INVALID_RESPONSE);
        }
        Ok(CatalogDetail {
            item,
            revision: wire.revision,
        })
    }

    pub async fn album_tracks(
        &self,
        session: &Session,
        id: &str,
        page: PageRequest,
    ) -> Result<CatalogPage, ApiError> {
        self.catalog_page(session, CatalogKind::Track, Parent::Album(id), page)
            .await
    }

    pub async fn artist_albums(
        &self,
        session: &Session,
        id: &str,
        page: PageRequest,
    ) -> Result<CatalogPage, ApiError> {
        self.catalog_page(session, CatalogKind::Album, Parent::Artist(id), page)
            .await
    }

    async fn catalog_page(
        &self,
        session: &Session,
        kind: CatalogKind,
        parent: Parent<'_>,
        page: PageRequest,
    ) -> Result<CatalogPage, ApiError> {
        if !limit_ok(page.limit) || page.offset > 1_000_000 {
            return Err(INVALID_INPUT);
        }
        let limit = page.limit.to_string();
        let offset = page.offset.to_string();
        let mut query = vec![
            ("kind", kind.plural()),
            ("scope", "all"),
            ("availability", "in-library"),
            ("limit", &limit),
            ("offset", &offset),
        ];
        match parent {
            Parent::Album(id) if valid_id(id) => query.push(("albumId", id)),
            Parent::Artist(id) if valid_id(id) => {
                query.extend([("artistId", id), ("releaseGroup", "all")])
            }
            _ => return Err(INVALID_INPUT),
        }
        let wire: WireCatalogPage = self.catalog_get(session, Route::Catalog, &query).await?;
        if !page_matches(page, wire.limit, wire.offset, wire.items.len(), wire.total)
            || wire.revision > i64::MAX as u64
        {
            return Err(INVALID_RESPONSE);
        }
        let items = project_items(wire.items, Some(kind), session.role == "admin")?;
        Ok(CatalogPage {
            items,
            total: wire.total,
            offset: wire.offset,
            limit: wire.limit,
            revision: wire.revision,
        })
    }

    /// Indexed, combined artist/album/track ranking. No implicit fallback or retry.
    pub async fn unified_search(
        &self,
        session: &Session,
        query: &str,
        kind: Option<CatalogKind>,
        limit: u32,
    ) -> Result<SearchPage, ApiError> {
        self.check_session(session)?;
        if !limit_ok(limit)
            || query.len() > 1000
            || query.chars().count() > 200
            || query.chars().any(char::is_control)
        {
            return Err(INVALID_INPUT);
        }
        let query = query.trim();
        if query.is_empty() {
            return Ok(SearchPage {
                items: vec![],
                result_count: 0,
                next_cursor: None,
                expires_at: String::new(),
            });
        }
        let limit_param = limit.to_string();
        let mut params = vec![("query", query), ("limit", limit_param.as_str())];
        if let Some(kind) = kind {
            params.push(("kind", kind.singular()));
        }
        self.search_page(session, &params, Some(limit), kind, None)
            .await
    }

    /// The original query, kind and size are retained by the server snapshot.
    pub async fn unified_search_next(
        &self,
        session: &Session,
        cursor: &str,
    ) -> Result<SearchPage, ApiError> {
        if !opaque_token(cursor, 512) {
            return Err(INVALID_INPUT);
        }
        self.search_page(session, &[("cursor", cursor)], None, None, Some(cursor))
            .await
    }

    async fn search_page(
        &self,
        session: &Session,
        params: &[(&str, &str)],
        limit: Option<u32>,
        kind: Option<CatalogKind>,
        cursor: Option<&str>,
    ) -> Result<SearchPage, ApiError> {
        let wire: WireSearchPage = self
            .catalog_get(session, Route::Search, params)
            .await
            .map_err(|err| match err.status {
                410 => ApiError {
                    message: "Search expired. Start a new search.",
                    ..err
                },
                503 => ApiError {
                    message: "Search is temporarily unavailable.",
                    ..err
                },
                _ => err,
            })?;
        wire.project(limit, kind, cursor)
    }
}

enum Parent<'a> {
    Album(&'a str),
    Artist(&'a str),
}

#[derive(Deserialize)]
struct WireCatalogPage {
    items: Vec<WireItem>,
    total: u64,
    offset: u32,
    limit: u32,
    revision: u64,
}

#[derive(Deserialize)]
struct WireCatalogDetail {
    item: WireItem,
    revision: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireSearchPage {
    items: Vec<WireItem>,
    result_count: u32,
    next_cursor: Option<String>,
    expires_at: String,
}

impl WireSearchPage {
    fn project(
        self,
        limit: Option<u32>,
        kind: Option<CatalogKind>,
        cursor: Option<&str>,
    ) -> Result<SearchPage, ApiError> {
        let count = self.items.len() as u32;
        if count > 100
            || count > self.result_count
            || !safe_timestamp(&self.expires_at)
            || self.next_cursor.as_deref().is_some_and(|next| {
                !opaque_token(next, 512)
                    || Some(next) == cursor
                    || count == 0
                    || count >= self.result_count
            })
            || limit.is_some_and(|limit| {
                count != limit.min(self.result_count)
                    || self.next_cursor.is_some() != (count < self.result_count)
            })
            || (cursor.is_some() && count == 0)
        {
            return Err(INVALID_RESPONSE);
        }
        let items = project_items(self.items, kind, false)?;
        Ok(SearchPage {
            items,
            result_count: self.result_count,
            next_cursor: self.next_cursor,
            expires_at: self.expires_at,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WireItem {
    kind: CatalogKind,
    id: String,
    name: Option<String>,
    #[serde(default)]
    artists: Vec<ArtistCredit>,
    #[serde(default)]
    genres: Vec<String>,
    #[serde(default)]
    album_id: String,
    #[serde(default)]
    album_name: String,
    #[serde(default)]
    album_type: String,
    #[serde(default)]
    release_date: String,
    #[serde(default)]
    relation: String,
    #[serde(default)]
    duration_ms: u64,
    #[serde(default)]
    disc_number: u32,
    #[serde(default)]
    track_number: u32,
    has_audio: Option<bool>,
    // Only the legacy administrator catalog projection omits `hasAudio`.
    // Use the presence of a recorded path as a boolean, never retain the path.
    #[serde(default, deserialize_with = "recorded_path")]
    library_path: bool,
    coverage: Coverage,
}

fn recorded_path<'de, D: serde::Deserializer<'de>>(de: D) -> Result<bool, D::Error> {
    Ok(Option::<String>::deserialize(de)?.is_some_and(|s| !s.is_empty()))
}

impl WireItem {
    pub(super) fn project(
        mut self,
        expected: Option<CatalogKind>,
        legacy_admin: bool,
        metadata_missing: bool,
    ) -> Result<CatalogItem, ApiError> {
        let optional_text = |s: &str, max| s.is_empty() || valid_text(s, max);
        // Go's legacy LibraryItem can omit an empty name. Positive search and
        // listener items emit the field even when no label has been saved.
        if self.name.is_none() && !legacy_admin && !metadata_missing {
            return Err(INVALID_RESPONSE);
        }
        let name = self.name.unwrap_or_default();
        if expected.is_some_and(|kind| kind != self.kind)
            || !valid_id(&self.id)
            || (if metadata_missing {
                !name.is_empty() || self.has_audio != Some(false)
            } else {
                !optional_text(&name, 1024)
            })
            || self.artists.iter().any(|a| {
                // Empty names are legal on catalog references with a saved ID.
                (!a.id.is_empty() && !valid_id(&a.id)) || !optional_text(&a.name, 1024)
            })
            || self.genres.len() > 128
            || self.genres.iter().any(|g| !valid_text(g, 1024))
            || (!self.album_id.is_empty() && !valid_id(&self.album_id))
            || !optional_text(&self.album_name, 1024)
            || !optional_text(&self.album_type, 128)
            || !optional_text(&self.release_date, 128)
            || !optional_text(&self.relation, 128)
            || self.duration_ms > 7 * 24 * 60 * 60 * 1000
            || self.track_number > 100_000
            || self.disc_number > 100_000
            || self.coverage.in_library > self.coverage.tracks
        {
            return Err(INVALID_RESPONSE);
        }
        let has_audio = match self.has_audio {
            Some(value) => value,
            None if legacy_admin => self.kind == CatalogKind::Track && self.library_path,
            None => return Err(INVALID_RESPONSE),
        };
        // The search writer accepts up to 256 credits within its byte limit;
        // a Library detail/listing can have more. Bound what we retain/display
        // without rejecting the whole valid release page on credit count.
        self.artists.truncate(256);
        Ok(CatalogItem {
            kind: self.kind,
            id: self.id,
            name,
            artists: self.artists,
            genres: self.genres,
            album_id: self.album_id,
            album_name: self.album_name,
            album_type: self.album_type,
            release_date: self.release_date,
            relation: self.relation,
            duration_ms: self.duration_ms,
            disc_number: self.disc_number,
            track_number: self.track_number,
            has_audio,
            coverage: self.coverage,
        })
    }
}

fn project_items(
    wire: Vec<WireItem>,
    expected: Option<CatalogKind>,
    legacy_admin: bool,
) -> Result<Vec<CatalogItem>, ApiError> {
    let items = wire
        .into_iter()
        .map(|item| item.project(expected, legacy_admin, false))
        .collect::<Result<Vec<_>, _>>()?;
    let mut seen = HashSet::new();
    if items
        .iter()
        .any(|item| !seen.insert((item.kind, item.id.as_str())))
    {
        return Err(INVALID_RESPONSE);
    }
    Ok(items)
}

pub(super) const fn limit_ok(limit: u32) -> bool {
    limit >= 1 && limit <= 100
}

pub(super) fn page_matches(
    page: PageRequest,
    limit: u32,
    offset: u32,
    len: usize,
    total: u64,
) -> bool {
    limit == page.limit
        && offset == page.offset
        && len <= limit as usize
        && (len == 0 || u64::from(offset) + len as u64 <= total)
}

pub(super) fn opaque_token(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && value.bytes().all(|b| b.is_ascii_graphic())
}

pub(super) fn safe_timestamp(value: &str) -> bool {
    let b = value.as_bytes();
    // RFC3339Nano from Go. Validate lexical shape without guessing clock skew.
    if b.len() < 20 || b.len() > 35 {
        return false;
    }
    for (i, c) in b[..19].iter().copied().enumerate() {
        if match i {
            4 | 7 => c != b'-',
            10 => c != b'T',
            13 | 16 => c != b':',
            _ => !c.is_ascii_digit(),
        } {
            return false;
        }
    }
    let mut tz = &b[19..];
    if let Some(fraction) = tz.strip_prefix(b".") {
        let digits = fraction.iter().take_while(|c| c.is_ascii_digit()).count();
        if !(1..=9).contains(&digits) {
            return false;
        }
        tz = &fraction[digits..];
    }
    tz == b"Z"
        || matches!(tz, [b'+' | b'-', h1, h2, b':', m1, m2] if [h1, h2, m1, m2].into_iter().all(|c| c.is_ascii_digit()))
}

/// Positive route list for both extension modules. Callers can supply opaque
/// path IDs or typed query parameters, but never an origin or an arbitrary path.
#[derive(Clone, Copy)]
pub(super) enum Route<'a> {
    Catalog,
    Detail(CatalogKind, &'a str),
    Search,
}

impl Route<'_> {
    fn url(self, api: &Api, query: &[(&str, &str)]) -> Result<Url, ApiError> {
        use Route::*;
        let mut url = api.origin.0.clone();
        {
            let mut path = url.path_segments_mut().map_err(|_| INVALID_INPUT)?;
            path.clear();
            match self {
                Catalog => {
                    path.extend(["api", "library"]);
                }
                Detail(kind, id) if segment_id(id) => {
                    path.extend(["api", "library", kind.plural(), id]);
                }
                Detail(..) => return Err(INVALID_INPUT),
                Search => {
                    path.push("search");
                }
            }
        }
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query.iter().copied());
        }
        Ok(url)
    }
}

// Go's oneSegment rejects path separators even in percent-encoded IDs.
pub(crate) fn segment_id(id: &str) -> bool {
    valid_id(id) && !id.contains(['/', '\\'])
}

pub(super) fn derived_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

const CATALOG_BODY: usize = 2 * 1024 * 1024;

impl Api {
    async fn catalog_get<T: DeserializeOwned>(
        &self,
        session: &Session,
        route: Route<'_>,
        query: &[(&str, &str)],
    ) -> Result<T, ApiError> {
        self.with_authenticated_network(session, async {
            let request = self
                .request(route.url(self, query)?)?
                .header(header::COOKIE, session.cookie()?.clone());
            let response = self.send(request, Some(session), false).await?;
            if response.status() != StatusCode::OK {
                return Err(INVALID_RESPONSE);
            }
            let value = decode(response, CATALOG_BODY).await?;
            self.check_session(session)?;
            Ok(value)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn wire_item(kind: &str, id: &str) -> serde_json::Value {
        json!({"kind": kind, "id": id, "name": "Synthetic music", "hasAudio": kind == "track",
            "artists": [{"id": "artist-a", "name": "Artist A"}, {"id": "", "name": "Name-only credit"}],
            "albumId": "album-a", "albumName": "Album A", "durationMs": 9400,
            "coverage": {"tracks": 1, "inLibrary": 1, "albums": 0}}
        )
    }

    #[test]
    fn route_ids_are_one_segment_and_query_is_form_encoded() {
        let api =
            Api::new(super::super::Origin::parse("https://catalog.invalid").unwrap()).unwrap();
        let url = Route::Detail(CatalogKind::Album, "test%2F id?#")
            .url(&api, &[])
            .unwrap();
        assert_eq!(
            url.as_str(),
            "https://catalog.invalid/api/library/albums/test%252F%20id%3F%23"
        );
        for id in [".", "..", "a/b", "a\\b", " id"] {
            assert!(
                Route::Detail(CatalogKind::Album, id)
                    .url(&api, &[])
                    .is_err()
            );
        }
        let url = Route::Search.url(&api, &[("cursor", "abc.+/_=&")]).unwrap();
        assert_eq!(url.path(), "/search");
        let pairs: Vec<_> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(pairs, [("cursor".to_owned(), "abc.+/_=&".to_owned())]);
    }

    #[test]
    fn catalog_keeps_navigation_metadata_and_projects_legacy_audio_only_for_admin() {
        let mut raw = wire_item("track", "track-a");
        raw.as_object_mut().unwrap().remove("hasAudio");
        raw["libraryPath"] = json!(r"synthetic/fixtures/escaped\name.mp3");
        let wire = || serde_json::from_value::<WireItem>(raw.clone()).unwrap();
        assert!(
            wire()
                .project(Some(CatalogKind::Track), false, false)
                .is_err()
        );
        let item = wire()
            .project(Some(CatalogKind::Track), true, false)
            .unwrap();
        assert_eq!(item.album_id, "album-a");
        assert_eq!(item.artists[0].id, "artist-a");
        assert!(item.artists[1].id.is_empty());
        assert!(item.has_audio);
        assert!(!format!("{item:?}").contains("fixtures/"));
    }

    #[test]
    fn go_can_open_a_credit_whose_artist_has_no_saved_name() {
        let raw = json!({"kind":"artist", "id":"guest-record", "name":"", "hasAudio":false,
            "coverage":{"tracks":0,"inLibrary":0,"albums":0}});
        let wire = |raw| serde_json::from_value::<WireItem>(raw).unwrap();
        let listener = wire(raw.clone())
            .project(Some(CatalogKind::Artist), false, false)
            .unwrap();
        assert_eq!(listener.id, "guest-record");
        assert!(listener.name.is_empty());

        let mut legacy = raw;
        let object = legacy.as_object_mut().unwrap();
        object.remove("name"); // LibraryItem uses omitempty, unlike ListeningItem.
        object.remove("hasAudio");
        assert_eq!(
            wire(legacy.clone())
                .project(Some(CatalogKind::Artist), true, false)
                .unwrap(),
            listener
        );
        assert!(
            wire(legacy)
                .project(Some(CatalogKind::Artist), false, false)
                .is_err()
        );
    }

    #[test]
    fn go_credit_budget_does_not_reject_a_release_page() {
        let mut raw = wire_item("album", "many-credits");
        // Valid within the Go search writer's 32 KiB / 256 credit budget;
        // it also exceeds the desktop's old 32 credit / 4096 name-byte caps.
        raw["artists"] = json!(
            (0..48)
                .map(|i| json!({"id":format!("artist-{i}"), "name":"x".repeat(128)}))
                .collect::<Vec<_>>()
        );
        let item = serde_json::from_value::<WireItem>(raw.clone())
            .unwrap()
            .project(Some(CatalogKind::Album), false, false)
            .unwrap();
        assert_eq!(item.artists.len(), 48);
        assert_eq!(item.artists.last().unwrap().id, "artist-47");

        // Library is not restricted to the search writer's credit count.
        raw["artists"] = json!(
            (0..300)
                .map(|i| json!({"id":format!("artist-{i}"), "name":""}))
                .collect::<Vec<_>>()
        );
        let item = serde_json::from_value::<WireItem>(raw)
            .unwrap()
            .project(Some(CatalogKind::Album), false, false)
            .unwrap();
        assert_eq!(item.artists.len(), 256);
        assert_eq!(item.artists.last().unwrap().id, "artist-255");
    }

    #[test]
    fn unified_search_preserves_server_order_and_rejects_bad_paging() {
        let mut raw = json!({"items":[wire_item("artist", "same"), wire_item("track", "same")],
            "resultCount":3, "nextCursor":"opaque-next", "expiresAt":"2030-01-02T03:04:05.123Z"});
        let read = |raw: &serde_json::Value| {
            serde_json::from_value::<WireSearchPage>(raw.clone()).unwrap()
        };
        let page = read(&raw).project(Some(2), None, None).unwrap();
        assert_eq!(
            page.items.iter().map(|i| i.kind).collect::<Vec<_>>(),
            [CatalogKind::Artist, CatalogKind::Track]
        );
        assert_eq!(page.next_cursor.as_deref(), Some("opaque-next"));
        assert!(
            read(&raw)
                .project(Some(2), Some(CatalogKind::Track), None)
                .is_err()
        );
        assert!(read(&raw).project(None, None, Some("opaque-next")).is_err());
        raw["items"][1] = wire_item("artist", "same");
        assert!(read(&raw).project(Some(2), None, None).is_err());
    }
}
