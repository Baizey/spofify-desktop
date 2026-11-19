//! Listener catalog and indexed search. Declare as a child of `api.rs` with
//! `#[path = "catalog.rs"] pub mod catalog;` so credentials and HTTP stay private.
//!
//! Browser interface on `Api` (`&Session` first): `catalog(kind, page)`,
//! `catalog_detail(kind, id)`, `album_tracks(id, page)`,
//! `artist_albums(id, release_group, page)`, `unified_search(query, kind, limit)`,
//! `unified_search_next(cursor)`. `kind` is `CatalogKind`, except search takes
//! `Option<CatalogKind>` (None = all). Owned Home/personal methods are in `library`.

use std::collections::HashSet;

use reqwest::{Method, Response, StatusCode, header};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use url::Url;

use super::{
    Api, ApiError, INVALID_INPUT, INVALID_RESPONSE, Item, Session, decode, valid_id, valid_text,
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReleaseGroup {
    Albums,
    Singles,
    Appearances,
    #[default]
    All,
}

impl ReleaseGroup {
    fn wire(self) -> &'static str {
        match self {
            Self::Albums => "albums",
            Self::Singles => "singles",
            Self::Appearances => "appearances",
            Self::All => "all",
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

impl CatalogPage {
    pub fn has_more(&self) -> bool {
        u64::from(self.offset) + (self.items.len() as u64) < self.total
    }
}

impl CatalogItem {
    /// Adapter for the existing single-track player. Do not turn an album or an
    /// unavailable row into playback authority. Collection identity stays with the row.
    pub fn playable_item(&self) -> Option<Item> {
        (self.kind == CatalogKind::Track && self.has_audio).then(|| Item {
            id: self.id.clone(),
            name: self.name.clone(),
            kind: self.kind.singular().into(),
            artists: self
                .artists
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            album_name: self.album_name.clone(),
            duration_ms: self.duration_ms,
            has_audio: true,
            track_number: self.track_number,
            disc_number: self.disc_number,
        })
    }
}

impl Api {
    pub async fn catalog(
        &self,
        session: &Session,
        kind: CatalogKind,
        page: PageRequest,
    ) -> Result<CatalogPage, ApiError> {
        self.catalog_page(session, kind, None, page).await
    }

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
        self.catalog_page(session, CatalogKind::Track, Some(Parent::Album(id)), page)
            .await
    }

    pub async fn artist_albums(
        &self,
        session: &Session,
        id: &str,
        release_group: ReleaseGroup,
        page: PageRequest,
    ) -> Result<CatalogPage, ApiError> {
        self.catalog_page(
            session,
            CatalogKind::Album,
            Some(Parent::Artist(id, release_group)),
            page,
        )
        .await
    }

    async fn catalog_page(
        &self,
        session: &Session,
        kind: CatalogKind,
        parent: Option<Parent<'_>>,
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
            Some(Parent::Album(id)) if valid_id(id) => query.push(("albumId", id)),
            Some(Parent::Artist(id, group)) if valid_id(id) => {
                query.extend([("artistId", id), ("releaseGroup", group.wire())])
            }
            Some(_) => return Err(INVALID_INPUT),
            None => (),
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
    Artist(&'a str, ReleaseGroup),
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
        if self.result_count > 300
            || count > 100
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
    #[serde(default)]
    name: String,
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
        self,
        expected: Option<CatalogKind>,
        legacy_admin: bool,
        metadata_missing: bool,
    ) -> Result<CatalogItem, ApiError> {
        let optional_text = |s: &str, max| s.is_empty() || valid_text(s, max);
        if expected.is_some_and(|kind| kind != self.kind)
            || !valid_id(&self.id)
            || (if metadata_missing {
                !self.name.is_empty() || self.has_audio != Some(false)
            } else {
                !valid_text(&self.name, 1024)
            })
            || self.artists.len() > 32
            || self
                .artists
                .iter()
                .any(|a| (!a.id.is_empty() && !valid_id(&a.id)) || !valid_text(&a.name, 1024))
            || self.artists.iter().map(|a| a.name.len() + 2).sum::<usize>() > 4096
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
        Ok(CatalogItem {
            kind: self.kind,
            id: self.id,
            name: self.name,
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
    Likes,
    LikeStatus,
    Recent,
    History,
    Playlists,
    Playlist(&'a str),
    Entries(&'a str),
    Entry(&'a str, &'a str),
    Order(&'a str),
    Definition(&'a str),
    Preview,
    Sources,
    Copy(&'a str),
}

impl Route<'_> {
    fn allows(self, method: &Method) -> bool {
        use Route::*;
        match self {
            Catalog | Detail(..) | Search | Recent | History | Sources => *method == Method::GET,
            Likes | Definition(_) => *method == Method::GET || *method == Method::PUT,
            Playlists | Entries(_) => *method == Method::GET || *method == Method::POST,
            Playlist(_) => {
                *method == Method::GET || *method == Method::PATCH || *method == Method::DELETE
            }
            LikeStatus | Preview | Copy(_) => *method == Method::POST,
            Entry(..) => *method == Method::DELETE,
            Order(_) => *method == Method::PUT,
        }
    }

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
                Likes => {
                    path.extend(["api", "me", "likes"]);
                }
                LikeStatus => {
                    path.extend(["api", "me", "likes", "status"]);
                }
                Recent => {
                    path.extend(["api", "me", "recent"]);
                }
                History => {
                    path.extend(["api", "me", "history"]);
                }
                Playlists => {
                    path.extend(["api", "me", "playlists"]);
                }
                Preview => {
                    path.extend(["api", "me", "playlists", "preview"]);
                }
                Sources => {
                    path.extend(["api", "me", "playlists", "sources"]);
                }
                Playlist(id)
                | Entries(id)
                | Entry(id, _)
                | Order(id)
                | Definition(id)
                | Copy(id) => {
                    if !segment_id(id) || matches!(id, "preview" | "sources") {
                        return Err(INVALID_INPUT);
                    }
                    path.extend(["api", "me", "playlists", id]);
                    match self {
                        Entries(_) => {
                            path.push("entries");
                        }
                        Entry(_, entry) if segment_id(entry) => {
                            path.extend(["entries", entry]);
                        }
                        Entry(..) => return Err(INVALID_INPUT),
                        Order(_) => {
                            path.push("order");
                        }
                        Definition(_) if derived_id(id) => {
                            path.push("definition");
                        }
                        Copy(_) if derived_id(id) => {
                            path.push("copy");
                        }
                        Definition(_) | Copy(_) => return Err(INVALID_INPUT),
                        _ => (),
                    }
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
fn segment_id(id: &str) -> bool {
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
const PERSONAL_REQUEST: usize = 64 * 1024;

impl Api {
    pub(super) async fn catalog_get<T: DeserializeOwned>(
        &self,
        session: &Session,
        route: Route<'_>,
        query: &[(&str, &str)],
    ) -> Result<T, ApiError> {
        self.with_authenticated_network(session, async {
            let response = self
                .catalog_response(session, route, Method::GET, query, None, StatusCode::OK)
                .await?;
            self.catalog_decode(session, response).await
        })
        .await
    }

    pub(super) async fn catalog_write<T: DeserializeOwned>(
        &self,
        session: &Session,
        route: Route<'_>,
        method: Method,
        body: impl Serialize,
        status: StatusCode,
    ) -> Result<T, ApiError> {
        self.with_authenticated_network(session, async {
            let response = self
                .catalog_response(
                    session,
                    route,
                    method,
                    &[],
                    Some(serialize_body(body)?),
                    status,
                )
                .await?;
            self.catalog_decode(session, response).await
        })
        .await
    }

    pub(super) async fn catalog_delete(
        &self,
        session: &Session,
        route: Route<'_>,
        body: impl Serialize,
    ) -> Result<(), ApiError> {
        self.with_authenticated_network(session, async {
            self.catalog_response(
                session,
                route,
                Method::DELETE,
                &[],
                Some(serialize_body(body)?),
                StatusCode::NO_CONTENT,
            )
            .await?;
            self.check_session(session)
        })
        .await
    }

    async fn catalog_decode<T: DeserializeOwned>(
        &self,
        session: &Session,
        response: Response,
    ) -> Result<T, ApiError> {
        let value = decode(response, CATALOG_BODY).await?;
        self.check_session(session)?;
        Ok(value)
    }

    async fn catalog_response(
        &self,
        session: &Session,
        route: Route<'_>,
        method: Method,
        query: &[(&str, &str)],
        body: Option<Vec<u8>>,
        status: StatusCode,
    ) -> Result<Response, ApiError> {
        self.check_session(session)?;
        let expects_body = method != Method::GET;
        if !route.allows(&method)
            || expects_body != body.is_some()
            || (expects_body && !query.is_empty())
        {
            return Err(INVALID_INPUT);
        }
        let mut request = self
            .request_with_method(method, route.url(self, query)?)?
            .header(header::COOKIE, session.secrets.cookie.clone());
        if let Some(body) = body {
            request = request
                .header("X-CSRF-Token", session.secrets.csrf.clone())
                .header(header::CONTENT_TYPE, "application/json")
                .body(body);
        }
        let response = self.send(request, Some(session), false).await?;
        if response.status() != status {
            return Err(INVALID_RESPONSE);
        }
        Ok(response)
    }
}

fn serialize_body(body: impl Serialize) -> Result<Vec<u8>, ApiError> {
    // Prevent oversized allocations even when escaping turns a small input into
    // a much larger JSON body. Paths never use a serialized mutation envelope.
    struct Bounded(Vec<u8>);
    impl std::io::Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > PERSONAL_REQUEST - self.0.len() {
                return Err(std::io::Error::other("request exceeds body limit"));
            }
            super::extend_bounded(&mut self.0, bytes, PERSONAL_REQUEST);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut output = Bounded(Vec::new());
    serde_json::to_writer(&mut output, &body).map_err(|_| INVALID_INPUT)?;
    Ok(output.0)
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
        for id in ["preview", "sources"] {
            assert!(Route::Playlist(id).url(&api, &[]).is_err());
        }
        let url = Route::Search.url(&api, &[("cursor", "abc.+/_=&")]).unwrap();
        assert_eq!(url.path(), "/search");
        let pairs: Vec<_> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(pairs, [("cursor".to_owned(), "abc.+/_=&".to_owned())]);

        let history = Route::History
            .url(&api, &[("limit", "20"), ("offset", "40")])
            .unwrap();
        assert_eq!(history.path(), "/api/me/history");
        assert_eq!(history.query(), Some("limit=20&offset=40"));
        assert!(Route::History.allows(&Method::GET));
        assert!(!Route::History.allows(&Method::DELETE));
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
        assert!(item.playable_item().is_some());
        assert!(!format!("{item:?}").contains("fixtures/"));
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

    #[test]
    fn mutation_serializer_caps_escaped_json() {
        let body = json!({"value": "\n".repeat(PERSONAL_REQUEST / 2)});
        assert_eq!(serialize_body(body), Err(INVALID_INPUT));
        assert_eq!(
            serialize_body(json!({"liked":true})).unwrap(),
            br#"{"liked":true}"#
        );
    }
}
