//! Playlist forms submit a JSON envelope with `name`, `description`, and a
//! `definition` string. The definition is human-readable, one directive per line;
//! it is not raw JSON. `prepare` returns the same fields for prefilling the form.
//!
//! Example:
//! ```text
//! Source: Liked tracks
//! Source: Saved library
//! Sort: Title
//! Duplicates: Keep one copy
//! Include:
//!   Any:
//!     Genre: jazz
//!     Genre: soul
//! Exclude:
//!   Availability: Unavailable
//! Limit: First 50
//! ```
//! Use two spaces per nesting level. Choices: Source = Liked tracks, Saved
//! library, Playlist <ID>, Tracked playlist <ID>; Sort = Source order or Title;
//! Duplicates = Keep all or Keep one copy; Limit = First/Last <count>. A source
//! can have its own indented Limit. Include and Exclude each take one condition:
//! All, Any, Not, Genre, Genre known (Yes/No), Artist <ID>, Album <ID>,
//! Availability (Available/Unavailable), or In (Liked tracks/Playlist <ID>/
//! Tracked playlist <ID>). Conditions with a value use `Label: value`.

use std::{collections::HashSet, fmt::Write as _};

use serde::Deserialize;
use uuid::Uuid;

use crate::{
    api::{
        ApiError,
        catalog::PageRequest,
        library::{
            PlaylistKind, RuleAvailability, RuleDefinition, RuleDuplicates, RuleMembership,
            RulePredicate, RuleSide, RuleSort, RuleSource, RuleTake,
        },
    },
    desk::Access,
    pages::{Page, PlaylistMetadata, Route, Row, RowKey},
};

pub const DEFAULT_RULES: &str =
    "Source: Liked tracks\nSort: Source order\nDuplicates: Keep one copy";
pub const RULES_HELP: &str = "Use one rule per line. Try ‘Source: Liked tracks’ or ‘Source: Saved library’. Optional: ‘Sort: Title’, ‘Duplicates: Keep one copy’, and ‘Limit: First 50’. Add ‘Include:’ on its own line, then ‘  Genre: jazz’ (two spaces). Use ‘Exclude:’ to leave matches out.";

const DRAFT_MAX: usize = 32 * 1024;
const STALE_VIEW: ApiError = ApiError {
    message: "That playlist changed elsewhere. Review its latest contents before editing it again.",
    expired: false,
    status: 409,
};

const fn error(message: &'static str) -> ApiError {
    ApiError {
        message,
        expired: false,
        status: 0,
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Draft {
    pub name: String,
    pub description: String,
    pub definition: String,
}

fn read_draft(value: &str) -> Result<Draft, ApiError> {
    // The form envelope may expand under JSON escaping even for short drafts.
    if value.len() > 6 * (DRAFT_MAX + 2400) + 100 {
        return Err(error("This playlist draft is too long."));
    }
    serde_json::from_str(value)
        .map_err(|_| error("Open the playlist form again and re-enter your changes."))
}

fn metadata(draft: &Draft) -> Result<(&str, &str), ApiError> {
    let name = draft.name.trim();
    let description = draft.description.as_str();
    if name.is_empty() || name.chars().count() > 100 || name.chars().any(char::is_control) {
        return Err(error(
            "Enter a playlist name of 1–100 characters on one line.",
        ));
    }
    if description.chars().count() > 500
        || description
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
    {
        return Err(error(
            "Use up to 500 characters for the description; avoid control characters.",
        ));
    }
    Ok((name, description))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceChoice<'a> {
    Likes,
    Saved,
    Playlist(&'a str),
    Tracked(&'a str),
}

impl SourceChoice<'_> {
    fn write(self, out: &mut String) {
        match self {
            Self::Likes => out.push_str("Liked tracks"),
            Self::Saved => out.push_str("Saved library"),
            Self::Playlist(id) => {
                let _ = write!(out, "Playlist {id}");
            }
            Self::Tracked(id) => {
                let _ = write!(out, "Tracked playlist {id}");
            }
        }
    }
}

fn source_parts(source: &RuleSource) -> (SourceChoice<'_>, &str, Option<RuleTake>) {
    match source {
        RuleSource::Likes { slot_id, take } => (SourceChoice::Likes, slot_id, *take),
        RuleSource::AllSaved { slot_id, take } => (SourceChoice::Saved, slot_id, *take),
        RuleSource::Playlist {
            slot_id,
            playlist_id,
            take,
        } => (SourceChoice::Playlist(playlist_id), slot_id, *take),
        RuleSource::TrackedPlaylist {
            slot_id,
            playlist_id,
            take,
        } => (SourceChoice::Tracked(playlist_id), slot_id, *take),
    }
}

fn parse_source(value: &str) -> Result<SourceChoice<'_>, ApiError> {
    let tail = |prefix: &str| {
        let head = value.get(..prefix.len())?;
        let rest = value.get(prefix.len()..)?.strip_prefix(' ')?;
        head.eq_ignore_ascii_case(prefix)
            .then(|| rest.trim_start_matches(' '))
    };
    if value.eq_ignore_ascii_case("Liked tracks") {
        Ok(SourceChoice::Likes)
    } else if value.eq_ignore_ascii_case("Saved library") {
        Ok(SourceChoice::Saved)
    } else if let Some(id) = tail("Playlist") {
        Ok(SourceChoice::Playlist(rule_id(id)?))
    } else if let Some(id) = tail("Tracked playlist") {
        Ok(SourceChoice::Tracked(rule_id(id)?))
    } else {
        Err(error(
            "Choose Liked tracks, Saved library, Playlist followed by its ID, or Tracked playlist followed by its ID.",
        ))
    }
}

fn rule_id(value: &str) -> Result<&str, ApiError> {
    if value.is_empty()
        || value.len() > 512
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(error(
            "Enter the ID of the playlist, artist, or album after its label.",
        ));
    }
    Ok(value)
}

fn amount(value: &str) -> Result<RuleTake, ApiError> {
    const BAD_LIMIT: ApiError =
        error("Write the limit as First or Last followed by a whole number from 1 to 2147483647.");
    let (side, count) = value.split_once(' ').ok_or(BAD_LIMIT)?;
    let count = count
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|count| (1..=i32::MAX as u32).contains(count))
        .ok_or(BAD_LIMIT)?;
    let side = if side.eq_ignore_ascii_case("first") {
        RuleSide::First
    } else if side.eq_ignore_ascii_case("last") {
        RuleSide::Last
    } else {
        return Err(BAD_LIMIT);
    };
    Ok(RuleTake { side, count })
}

const BAD_INDENT: ApiError =
    error("Use two spaces for each level of rules, and place conditions below Include or Exclude.");

#[derive(Clone, Copy)]
struct Line<'a> {
    level: usize,
    label: &'a str,
    value: &'a str,
}

struct RecipeParser<'a> {
    lines: std::iter::Peekable<std::vec::IntoIter<Line<'a>>>,
    leaves: usize,
}

impl<'a> RecipeParser<'a> {
    fn new(text: &'a str) -> Result<Self, ApiError> {
        if text.len() > DRAFT_MAX {
            return Err(error("These rules are too long."));
        }
        let mut lines = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let line = line.trim_end();
            let content = line.trim_start_matches(' ');
            let spaces = line.len() - content.len();
            if spaces % 2 != 0 || spaces > 8 || content.starts_with(char::is_whitespace) {
                return Err(BAD_INDENT);
            }
            let (label, value) = content.split_once(':').ok_or(error(
                "Put a colon after each rule label, for example ‘Genre: jazz’.",
            ))?;
            lines.push(Line {
                level: spaces / 2,
                label: label.trim_end(),
                value: value.trim(),
            });
        }
        Ok(Self {
            lines: lines.into_iter().peekable(),
            leaves: 0,
        })
    }

    fn has_child(&mut self, level: usize) -> bool {
        self.lines.peek().is_some_and(|line| line.level > level)
    }

    fn parse(mut self, previous: Option<&RuleDefinition>) -> Result<RuleDefinition, ApiError> {
        let mut choices = Vec::new();
        let (mut sort, mut duplicates, mut take, mut include, mut exclude) =
            (None, None, None, None, None);
        while let Some(Line {
            level,
            label,
            value,
        }) = self.lines.next()
        {
            if level != 0 {
                return Err(BAD_INDENT);
            }
            match label.to_ascii_lowercase().as_str() {
                "source" => {
                    if choices.len() == 4 {
                        return Err(error("Choose up to four sources."));
                    }
                    let choice = parse_source(value)?;
                    let source_take = if self.has_child(0) {
                        let child = self.lines.next().expect("peeked child");
                        if child.level != 1 || !child.label.eq_ignore_ascii_case("limit") {
                            return Err(error(
                                "Only an indented Limit belongs directly below Source.",
                            ));
                        }
                        Some(amount(child.value)?)
                    } else {
                        None
                    };
                    choices.push((choice, source_take));
                }
                "sort" if sort.is_none() => {
                    sort = Some(if value.eq_ignore_ascii_case("source order") {
                        RuleSort::SourceOrder
                    } else if value.eq_ignore_ascii_case("title") {
                        RuleSort::Title
                    } else {
                        return Err(error("Choose ‘Sort: Source order’ or ‘Sort: Title’."));
                    });
                }
                "duplicates" if duplicates.is_none() => {
                    duplicates = Some(if value.eq_ignore_ascii_case("keep all") {
                        RuleDuplicates::Keep
                    } else if value.eq_ignore_ascii_case("keep one copy") {
                        RuleDuplicates::Unique
                    } else {
                        return Err(error(
                            "Choose ‘Duplicates: Keep all’ or ‘Duplicates: Keep one copy’.",
                        ));
                    });
                }
                "limit" if take.is_none() => take = Some(amount(value)?),
                "include" if include.is_none() => include = Some(self.section(value)?),
                "exclude" if exclude.is_none() => exclude = Some(self.section(value)?),
                "sort" | "duplicates" | "limit" | "include" | "exclude" => {
                    return Err(error(
                        "Write only one of each Sort, Duplicates, Limit, Include, and Exclude section.",
                    ));
                }
                _ => {
                    return Err(error(
                        "At the start of a line use Source, Sort, Duplicates, Limit, Include, or Exclude.",
                    ));
                }
            }
        }
        if choices.is_empty() {
            return Err(error(
                "Add at least one source, for example ‘Source: Liked tracks’.",
            ));
        }
        Ok(RuleDefinition {
            version: 1,
            sources: assign_slots(choices, previous),
            sort: sort.unwrap_or(RuleSort::SourceOrder),
            duplicates: duplicates.unwrap_or(RuleDuplicates::Unique),
            take,
            include,
            exclude,
        })
    }

    fn section(&mut self, value: &str) -> Result<RulePredicate, ApiError> {
        const BAD_SECTION: ApiError = error(
            "Place one indented condition below Include or Exclude. Use All or Any to group several conditions.",
        );
        if !value.is_empty() || !self.has_child(0) {
            return Err(BAD_SECTION);
        }
        let condition = self.condition(1)?;
        if self.has_child(0) {
            return Err(BAD_SECTION);
        }
        Ok(condition)
    }

    fn condition(&mut self, level: usize) -> Result<RulePredicate, ApiError> {
        let Line {
            level: actual,
            label,
            value,
        } = self.lines.next().ok_or(BAD_INDENT)?;
        if level != actual || level > 4 {
            return Err(BAD_INDENT);
        }
        let label = label.to_ascii_lowercase();
        match label.as_str() {
            "all" | "any" | "not" => {
                if !value.is_empty() || !self.has_child(level) || level == 4 {
                    return Err(error(
                        "Put indented conditions below All, Any, or Not; conditions can nest up to four levels.",
                    ));
                }
                let arg = self.condition(level + 1)?;
                if label == "not" {
                    if self.has_child(level) {
                        return Err(error("Not takes exactly one condition."));
                    }
                    return Ok(RulePredicate::Not { arg: Box::new(arg) });
                }
                let mut args = vec![arg];
                while self.has_child(level) {
                    args.push(self.condition(level + 1)?);
                }
                return Ok(if label == "all" {
                    RulePredicate::All { args }
                } else {
                    RulePredicate::Any { args }
                });
            }
            _ => {}
        }
        self.leaves += 1;
        if self.leaves > 32 {
            return Err(error("Use up to 32 conditions across Include and Exclude."));
        }
        let condition = match label.as_str() {
            "genre"
                if !value.is_empty()
                    && value.len() <= 200
                    && value.chars().count() <= 100
                    && !value.contains(',')
                    && !value.chars().any(char::is_control) =>
            {
                RulePredicate::Genre {
                    value: value.to_owned(),
                }
            }
            "genre" => {
                return Err(error(
                    "Enter a single genre, for example ‘Genre: jazz’. Use Any to match several genres.",
                ));
            }
            "genre known"
                if value.eq_ignore_ascii_case("yes") || value.eq_ignore_ascii_case("no") =>
            {
                RulePredicate::GenreKnown {
                    value: value.eq_ignore_ascii_case("yes"),
                }
            }
            "genre known" => return Err(error("Write ‘Genre known: Yes’ or ‘Genre known: No’.")),
            "artist" => RulePredicate::Artist {
                artist_id: rule_id(value)?.to_owned(),
            },
            "album" => RulePredicate::Album {
                album_id: rule_id(value)?.to_owned(),
            },
            "availability"
                if value.eq_ignore_ascii_case("available")
                    || value.eq_ignore_ascii_case("unavailable") =>
            {
                RulePredicate::Availability {
                    value: if value.eq_ignore_ascii_case("available") {
                        RuleAvailability::Available
                    } else {
                        RuleAvailability::Unavailable
                    },
                }
            }
            "availability" => {
                return Err(error(
                    "Choose ‘Availability: Available’ or ‘Availability: Unavailable’.",
                ));
            }
            "in" => RulePredicate::Membership {
                source: match parse_source(value)? {
                    SourceChoice::Likes => RuleMembership::Likes,
                    SourceChoice::Playlist(id) => RuleMembership::Playlist {
                        playlist_id: id.to_owned(),
                    },
                    SourceChoice::Tracked(id) => RuleMembership::TrackedPlaylist {
                        playlist_id: id.to_owned(),
                    },
                    SourceChoice::Saved => {
                        return Err(error(
                            "Choose Liked tracks or a playlist for an In condition.",
                        ));
                    }
                },
            },
            _ => {
                return Err(error(
                    "Unknown condition. Choose All, Any, Not, Genre, Genre known, Artist, Album, Availability, or In.",
                ));
            }
        };
        if self.has_child(level) {
            return Err(BAD_INDENT);
        }
        Ok(condition)
    }
}

// A source slot is a server identity, not just a line number. First reserve
// unchanged sources (including limits), then preserve the slot of sources whose
// limits were edited. Never reuse another kind's slot or one already used here.
fn assign_slots(
    choices: Vec<(SourceChoice<'_>, Option<RuleTake>)>,
    previous: Option<&RuleDefinition>,
) -> Vec<RuleSource> {
    let originals: Vec<_> = previous
        .into_iter()
        .flat_map(|d| &d.sources)
        .map(source_parts)
        .collect();
    let mut used = HashSet::new();
    let mut assignments = vec![None; choices.len()];
    for exact in [true, false] {
        for ((choice, take), slot) in choices.iter().zip(&mut assignments) {
            if slot.is_some() {
                continue;
            }
            if let Some((_, id, _)) = originals.iter().find(|(old_choice, id, old_take)| {
                old_choice == choice && (!exact || old_take == take) && !used.contains(id)
            }) {
                used.insert(*id);
                *slot = Some((*id).to_owned());
            }
        }
    }
    choices
        .into_iter()
        .zip(assignments)
        .map(|((choice, take), slot)| {
            let slot_id = slot.unwrap_or_else(|| format!("s_{}", Uuid::new_v4().simple()));
            match choice {
                SourceChoice::Likes => RuleSource::Likes { slot_id, take },
                SourceChoice::Saved => RuleSource::AllSaved { slot_id, take },
                SourceChoice::Playlist(id) => RuleSource::Playlist {
                    slot_id,
                    playlist_id: id.to_owned(),
                    take,
                },
                SourceChoice::Tracked(id) => RuleSource::TrackedPlaylist {
                    slot_id,
                    playlist_id: id.to_owned(),
                    take,
                },
            }
        })
        .collect()
}

pub fn format_definition(definition: &RuleDefinition) -> String {
    let mut out = String::new();
    for source in &definition.sources {
        let (choice, _, take) = source_parts(source);
        out.push_str("Source: ");
        choice.write(&mut out);
        out.push('\n');
        if let Some(take) = take {
            write_limit(&mut out, "  ", take);
        }
    }
    let _ = writeln!(
        out,
        "Sort: {}",
        match definition.sort {
            RuleSort::SourceOrder => "Source order",
            RuleSort::Title => "Title",
        }
    );
    let _ = writeln!(
        out,
        "Duplicates: {}",
        match definition.duplicates {
            RuleDuplicates::Keep => "Keep all",
            RuleDuplicates::Unique => "Keep one copy",
        }
    );
    for (label, condition) in [
        ("Include", &definition.include),
        ("Exclude", &definition.exclude),
    ] {
        if let Some(condition) = condition {
            let _ = writeln!(out, "{label}:");
            write_condition(&mut out, 1, condition);
        }
    }
    if let Some(take) = definition.take {
        write_limit(&mut out, "", take);
    }
    out.trim_end().to_owned()
}

fn write_limit(out: &mut String, indent: &str, take: RuleTake) {
    let side = match take.side {
        RuleSide::First => "First",
        RuleSide::Last => "Last",
    };
    let _ = writeln!(out, "{indent}Limit: {side} {}", take.count);
}

/// Prefill from the retained page. Do not issue a second read when opening a
/// form: the edited definition and expected revision must describe one snapshot.
pub fn prepare(route: &Route, page: &Page, op: &str) -> Result<Draft, ApiError> {
    match op.strip_prefix("prepare-").unwrap_or(op) {
        "new-manual" => Ok(Draft::default()),
        "new-rules" => Ok(Draft {
            definition: DEFAULT_RULES.into(),
            ..Draft::default()
        }),
        "rename" => {
            let playlist = current_playlist(route, page)?;
            require(
                playlist.capabilities.edit_metadata,
                "You can’t change this playlist’s details.",
            )?;
            Ok(Draft {
                name: playlist.name.clone(),
                description: page.description.clone(),
                ..Draft::default()
            })
        }
        "rules" => {
            let playlist = current_playlist(route, page)?;
            let definition = current_definition(route, page)?;
            Ok(Draft {
                name: playlist.name.clone(),
                description: page.description.clone(),
                definition: format_definition(definition),
            })
        }
        _ => Err(error("This playlist form is not available.")),
    }
}

/// Executes exactly one write at most. A conflict is never replayed with a
/// newer revision. The caller can re-read affected views on status 409; account
/// and navigation epoch checks on the returned result belong to the caller.
pub async fn mutate(
    access: &Access,
    route: &Route,
    page: &Page,
    op: &str,
    index: Option<usize>,
    value: &str,
) -> Result<Option<Route>, ApiError> {
    const OFFLINE: ApiError = error("Go online to make changes to your library.");
    let mut network = access.session.network_changes();
    if access.offline || *network.borrow_and_update() & 1 != 0 {
        return Err(OFFLINE);
    }
    tokio::select! {
        biased;
        _ = network.changed() => Err(OFFLINE),
        result = apply(access, route, page, op, index, value) => result,
    }
}

async fn apply(
    access: &Access,
    route: &Route,
    page: &Page,
    op: &str,
    index: Option<usize>,
    value: &str,
) -> Result<Option<Route>, ApiError> {
    let api = &access.api;
    let session = &access.session;
    match op {
        "new-manual" | "new-rules" => {
            let draft = read_draft(value)?;
            let (name, description) = metadata(&draft)?;
            let definition = (op == "new-rules")
                .then(|| RecipeParser::new(&draft.definition)?.parse(None))
                .transpose()?;
            let created = api
                .create_playlist(session, name, description, definition.as_ref())
                .await?;
            return Ok(Some(Route::Playlist(created.id)));
        }
        "rename" => {
            let playlist = current_playlist(route, page)?;
            require(
                playlist.capabilities.edit_metadata,
                "You can’t change this playlist’s details.",
            )?;
            let draft = read_draft(value)?;
            let (name, description) = metadata(&draft)?;
            api.update_playlist(session, &playlist.id, playlist.revision, name, description)
                .await?;
        }
        "rules" => {
            let playlist = current_playlist(route, page)?;
            let previous = current_definition(route, page)?;
            let draft = read_draft(value)?;
            let definition = RecipeParser::new(&draft.definition)?.parse(Some(previous))?;
            api.update_playlist_definition(session, &playlist.id, playlist.revision, &definition)
                .await?;
        }
        "delete" => {
            // The UI owns confirmation. Only a playlist detail can be deleted.
            let playlist = current_playlist(route, page)?;
            require(
                playlist.capabilities.delete,
                "You can’t delete this playlist.",
            )?;
            api.delete_playlist(session, &playlist.id, playlist.revision)
                .await?;
            return Ok(Some(Route::Playlists));
        }
        "like" | "unlike" | "remove" if op != "remove" || matches!(route, Route::Liked) => {
            let (track, liked) = like_change(route, page, index, op == "like")?;
            api.set_like(session, track, liked).await?;
        }
        "add" => {
            let (track, playlist) = add_target(route, page, index)?;
            api.add_playlist_track(session, &playlist.id, playlist.revision, track)
                .await?;
            return Ok(Some(Route::Playlist(playlist.id.clone())));
        }
        "remove" | "up" | "down" => {
            let playlist = current_playlist(route, page)?;
            require(
                playlist.kind == PlaylistKind::Manual && playlist.capabilities.edit_entries,
                "Only tracks in a manual playlist can be removed or moved.",
            )?;
            let entry = entry_id(selected(page, index)?, &playlist.id)?;
            if op == "remove" {
                api.remove_playlist_entry(session, &playlist.id, playlist.revision, entry)
                    .await?;
            } else {
                let up = op == "up";
                let mut order = retained_order(page, &playlist.id)?;
                let total = page
                    .total
                    .or(playlist.available_count)
                    .or(playlist.track_count);
                let complete = !page.more && total.is_some_and(|total| order.len() as u64 == total);
                if !complete {
                    // This point-to-point read only runs when an explicit edit needs
                    // the server's complete permutation, never on a timer or for nav.
                    // Check the visible prefix against the result before writing.
                    let mut fetched = Vec::new();
                    loop {
                        const LIMIT: u32 = 100;
                        // The 64 KiB real server request cannot fit this many
                        // generated 32-character entry IDs even if every ID is ASCII.
                        const MAX_ENTRIES: u64 = (64 * 1024 - 64) / 35;
                        if total.is_some_and(|total| total > MAX_ENTRIES) {
                            return Err(error(
                                "This playlist has too many tracks to reorder at once.",
                            ));
                        }
                        let offset = fetched.len() as u32;
                        let batch = api
                            .manual_entries(
                                session,
                                &playlist.id,
                                PageRequest {
                                    offset,
                                    limit: LIMIT,
                                },
                            )
                            .await?;
                        if total.is_some_and(|total| batch.total != total) {
                            return Err(STALE_VIEW);
                        }
                        if batch.total > MAX_ENTRIES {
                            return Err(error(
                                "This playlist has too many tracks to reorder at once.",
                            ));
                        }
                        let more =
                            u64::from(batch.offset) + (batch.items.len() as u64) < batch.total;
                        if batch.offset != offset || more && batch.items.is_empty() {
                            return Err(STALE_VIEW);
                        }
                        fetched.extend(batch.items.into_iter().map(|item| item.id));
                        if !more {
                            break;
                        }
                    }
                    if !fetched.starts_with(&order) {
                        return Err(STALE_VIEW);
                    }
                    order = fetched;
                }
                if move_entry(&mut order, entry, up)? {
                    api.reorder_playlist(session, &playlist.id, playlist.revision, &order)
                        .await?;
                }
            }
        }
        _ => return Err(error("That library action is not available here.")),
    }
    Ok(None)
}

fn require(condition: bool, message: &'static str) -> Result<(), ApiError> {
    if condition {
        Ok(())
    } else {
        Err(error(message))
    }
}

fn current_playlist<'a>(route: &Route, page: &'a Page) -> Result<&'a PlaylistMetadata, ApiError> {
    let Route::Playlist(id) = route else {
        return Err(error("Open a playlist before editing it."));
    };
    page.playlist
        .as_ref()
        .filter(|playlist| &playlist.id == id && playlist.revision > 0)
        .ok_or(STALE_VIEW)
}

fn current_definition<'a>(route: &Route, page: &'a Page) -> Result<&'a RuleDefinition, ApiError> {
    let playlist = current_playlist(route, page)?;
    require(
        playlist.kind == PlaylistKind::Derived && playlist.capabilities.edit_definition,
        "This playlist doesn’t have editable rules.",
    )?;
    page.definition
        .as_ref()
        .ok_or(error("The rules aren’t available yet."))
}

fn selected(page: &Page, index: Option<usize>) -> Result<&Row, ApiError> {
    index
        .and_then(|index| page.rows.get(index))
        .ok_or(error("Select an item first."))
}

fn like_change<'a>(
    route: &'a Route,
    page: &'a Page,
    index: Option<usize>,
    like: bool,
) -> Result<(&'a str, bool), ApiError> {
    let (track, known) = like_target(route, page, index)?;
    // Like is an explicit SET, even for an already-liked track or when a read
    // has not confirmed membership. Unlike is offered only for confirmed likes.
    require(
        like || known == Some(true),
        "This track isn’t marked as liked.",
    )?;
    Ok((track, like))
}

fn like_target<'a>(
    route: &'a Route,
    page: &'a Page,
    index: Option<usize>,
) -> Result<(&'a str, Option<bool>), ApiError> {
    if let Some(index) = index {
        let row = selected(page, Some(index))?;
        let is_track = match &row.key {
            RowKey::Track(id) => id == &row.id,
            RowKey::HistoryEvent(_)
            | RowKey::ManualEntry { .. }
            | RowKey::DerivedOccurrence { .. } => true,
            _ => false,
        };
        if is_track && !row.id.is_empty() {
            return Ok((
                &row.id,
                row.liked.or(matches!(route, Route::Liked).then_some(true)),
            ));
        }
    } else if let Route::Track(id) = route {
        if !id.is_empty() {
            let known = page
                .rows
                .iter()
                .find(|row| matches!(&row.key, RowKey::Track(key) if key == id) && &row.id == id)
                .and_then(|row| row.liked);
            return Ok((id, known));
        }
    }
    Err(error("Select a track first."))
}

fn add_target<'a>(
    route: &'a Route,
    page: &'a Page,
    index: Option<usize>,
) -> Result<(&'a str, &'a PlaylistMetadata), ApiError> {
    let Route::Picker(track) = route else {
        return Err(error("Choose a playlist to add the track to."));
    };
    if track.is_empty() || page.picker_track_id.as_ref().is_some_and(|id| id != track) {
        return Err(error("Select the track again before adding it."));
    }
    let row = selected(page, index)?;
    let playlist = row
        .playlist
        .as_ref()
        .filter(|playlist| {
            matches!(&row.key, RowKey::Playlist(id) if id == &playlist.id) && playlist.revision > 0
        })
        .ok_or(error("Select a playlist first."))?;
    require(
        playlist.kind == PlaylistKind::Manual && playlist.capabilities.add_tracks,
        "Only manual playlists can accept individual tracks.",
    )?;
    Ok((track, playlist))
}

fn entry_id<'a>(row: &'a Row, list_id: &str) -> Result<&'a str, ApiError> {
    match &row.key {
        RowKey::ManualEntry {
            playlist_id,
            entry_id,
        } if playlist_id == list_id && !entry_id.is_empty() => Ok(entry_id),
        _ => Err(error("Select a track from this manual playlist.")),
    }
}

fn retained_order(page: &Page, playlist_id: &str) -> Result<Vec<String>, ApiError> {
    page.rows
        .iter()
        .map(|row| entry_id(row, playlist_id).map(str::to_owned))
        .collect()
}

fn move_entry(order: &mut [String], entry: &str, up: bool) -> Result<bool, ApiError> {
    let mut seen = HashSet::new();
    if order.iter().any(|id| id.is_empty() || !seen.insert(id)) {
        return Err(STALE_VIEW);
    }
    let current = order.iter().position(|id| id == entry).ok_or(STALE_VIEW)?;
    let next = if up {
        current.checked_sub(1)
    } else {
        current.checked_add(1)
    };
    let Some(next) = next.filter(|&index| index < order.len()) else {
        return Ok(false);
    };
    order.swap(current, next);
    Ok(true)
}

fn write_condition(out: &mut String, level: usize, condition: &RulePredicate) {
    for _ in 0..level {
        out.push_str("  ");
    }
    match condition {
        RulePredicate::All { args } | RulePredicate::Any { args } => {
            let label = if matches!(condition, RulePredicate::All { .. }) {
                "All"
            } else {
                "Any"
            };
            let _ = writeln!(out, "{label}:");
            for arg in args {
                write_condition(out, level + 1, arg);
            }
        }
        RulePredicate::Not { arg } => {
            out.push_str("Not:\n");
            write_condition(out, level + 1, arg);
        }
        RulePredicate::Genre { value } => {
            let _ = writeln!(out, "Genre: {value}");
        }
        RulePredicate::GenreKnown { value } => {
            let _ = writeln!(out, "Genre known: {}", if *value { "Yes" } else { "No" });
        }
        RulePredicate::Artist { artist_id } => {
            let _ = writeln!(out, "Artist: {artist_id}");
        }
        RulePredicate::Album { album_id } => {
            let _ = writeln!(out, "Album: {album_id}");
        }
        RulePredicate::Availability { value } => {
            let _ = writeln!(
                out,
                "Availability: {}",
                match value {
                    RuleAvailability::Available => "Available",
                    RuleAvailability::Unavailable => "Unavailable",
                }
            );
        }
        RulePredicate::Membership { source } => {
            out.push_str("In: ");
            match source {
                RuleMembership::Likes => SourceChoice::Likes,
                RuleMembership::Playlist { playlist_id } => SourceChoice::Playlist(playlist_id),
                RuleMembership::TrackedPlaylist { playlist_id } => {
                    SourceChoice::Tracked(playlist_id)
                }
            }
            .write(out);
            out.push('\n');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Api, Origin, library::PlaylistCapabilities};
    use serde_json::{Value, json};
    use std::{future::Future, sync::Arc, time::Duration};
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
    };

    fn parse(text: &str) -> Result<RuleDefinition, ApiError> {
        RecipeParser::new(text)?.parse(None)
    }

    #[test]
    fn friendly_default_builds_real_version_one_definition() {
        let recipe = parse(DEFAULT_RULES).unwrap();
        let wire = serde_json::to_value(&recipe).unwrap();
        assert_eq!(wire["version"], 1);
        assert_eq!(wire["duplicates"], "unique");
        assert_eq!(wire["sort"], "sourceOrder");
        assert_eq!(wire["sources"][0]["kind"], "likes");
        let slot = wire["sources"][0]["slotId"].as_str().unwrap();
        assert_eq!(slot.len(), 34);
        assert!(slot.starts_with("s_"));
        assert!(
            slot[2..]
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        assert_eq!(format_definition(&recipe), DEFAULT_RULES);
    }

    #[test]
    fn existing_definition_round_trips_through_guided_editor_without_identity_loss() {
        let input = "Source: Playlist own:playlist\n  Limit: First 10\nSource: Tracked playlist spotify-id\nSource: Saved library\nSource: Liked tracks\nInclude:\n  All:\n    Genre: jazz fusion\n    Any:\n      Artist: artist:123\n      Album: album:789\n    Not:\n      Genre known: No\nExclude:\n  Any:\n    In: Tracked playlist spotify-id\n    In: Playlist other-id\n    In: Liked tracks\n    Availability: Unavailable\nSort: Title\nDuplicates: Keep all\nLimit: Last 100";
        let recipe = parse(input).unwrap();
        let out = format_definition(&recipe);
        let again = RecipeParser::new(&out)
            .unwrap()
            .parse(Some(&recipe))
            .unwrap();
        assert_eq!(again, recipe);
        let wire = serde_json::to_value(&recipe).unwrap();
        assert_eq!(wire["sources"][0]["playlistId"], "own:playlist");
        assert_eq!(
            wire["sources"][0]["take"],
            json!({"side":"first","count":10})
        );
        assert_eq!(
            wire["include"]["args"][1]["args"][0],
            json!({"op":"artist","artistId":"artist:123"})
        );
        assert_eq!(
            wire["include"]["args"][2]["arg"],
            json!({"op":"genreKnown", "value":false})
        );
        assert_eq!(
            wire["exclude"]["args"][0],
            json!({"op":"membership","source":{"kind":"trackedPlaylist","playlistId":"spotify-id"}})
        );
    }

    #[test]
    fn preserving_duplicate_source_slots_prefers_the_unchanged_limit() {
        let previous = parse("Source: Liked tracks\n  Limit: First 5\nSource: Liked tracks\n  Limit: First 10\nSource: Playlist removed").unwrap();
        let edited = RecipeParser::new("Source: Liked tracks\n  Limit: First 10\nSource: Liked tracks\n  Limit: First 15\nSource: Playlist new")
            .unwrap().parse(Some(&previous)).unwrap();
        assert_eq!(
            source_parts(&edited.sources[0]).1,
            source_parts(&previous.sources[1]).1
        );
        assert_eq!(
            source_parts(&edited.sources[1]).1,
            source_parts(&previous.sources[0]).1
        );
        assert_ne!(
            source_parts(&edited.sources[2]).1,
            source_parts(&previous.sources[2]).1
        );
    }

    #[test]
    fn invalid_directives_and_structure_do_not_fall_back_to_broader_rules() {
        for body in [
            "",
            "Source: unknown",
            "Source: Liked tracks\nSort: Shuffle",
            "Source: Liked tracks\nSort: Title\nSort: Source order",
            "Source: Liked tracks\nInclude:\n Genre: jazz",
            "Source: Liked tracks\nInclude:\n  Not:\n    Genre: jazz\n    Genre: rock",
            "Source: Liked tracks\nInclude:\n  All:\n    All:\n      All:\n        All:\n          Genre: jazz",
            "Source: Liked tracks\nInclude:\n  Genre: jazz\n  Genre: rock",
            "Source: Liked tracks\nExclude:\n  In: Saved library",
            "Source: Liked tracks\nLimit: Last 2147483648",
            "Source: Liked tracks\n  Sort: Title",
        ] {
            assert!(parse(body).is_err(), "accepted {body:?}");
        }
        assert!(parse(&"Source: Liked tracks\n".repeat(5)).is_err());
        assert!(
            parse(&format!(
                "Source: Liked tracks\nInclude:\n  Any:\n{}",
                "    Genre: jazz\n".repeat(33)
            ))
            .is_err()
        );
    }

    fn playlist(kind: PlaylistKind) -> PlaylistMetadata {
        let manual = kind == PlaylistKind::Manual;
        PlaylistMetadata {
            id: "list".into(),
            name: "A useful playlist".into(),
            kind,
            revision: 4,
            track_count: Some(2),
            available_count: Some(2),
            capabilities: PlaylistCapabilities {
                edit_metadata: true,
                edit_entries: manual,
                edit_definition: !manual,
                play: true,
                copy_to_manual: !manual,
                delete: true,
                add_tracks: manual,
            },
        }
    }

    #[test]
    fn forms_prefill_existing_details_without_defaulting_missing_rules() {
        let route = Route::Playlist("list".into());
        let definition = parse("Source: Saved library\nLimit: First 50").unwrap();
        let mut page = Page {
            title: "Rendered title".into(),
            description: "Existing description".into(),
            playlist: Some(playlist(PlaylistKind::Derived)),
            definition: Some(definition.clone()),
            ..Page::default()
        };
        let rename = prepare(&route, &page, "prepare-rename").unwrap();
        assert_eq!(rename.name, "A useful playlist");
        assert_eq!(rename.description, page.description);
        assert_eq!(
            prepare(&route, &page, "rules").unwrap().definition,
            format_definition(&definition)
        );
        page.definition = None;
        assert!(prepare(&route, &page, "rules").is_err());
        assert_eq!(
            prepare(&Route::Playlists, &Page::default(), "new-rules")
                .unwrap()
                .definition,
            DEFAULT_RULES
        );
    }

    #[test]
    fn reorder_swaps_occurrences_even_if_the_track_appears_twice() {
        let row = |id: &str| Row {
            id: "same-track".into(),
            key: RowKey::ManualEntry {
                playlist_id: "list".into(),
                entry_id: id.into(),
            },
            ..Row::default()
        };
        let page = Page {
            rows: vec![row("entry-1"), row("entry-2")],
            ..Page::default()
        };
        let mut order = retained_order(&page, "list").unwrap();
        assert!(move_entry(&mut order, "entry-2", true).unwrap());
        assert_eq!(order, ["entry-2", "entry-1"]);
        assert!(!move_entry(&mut order, "entry-1", false).unwrap());
        assert!(move_entry(&mut order, "same-track", true).is_err());
        assert!(retained_order(&page, "other-list").is_err());
    }

    #[test]
    fn picker_uses_typed_manual_target_and_originating_track() {
        let row = Row {
            key: RowKey::Playlist("list".into()),
            playlist: Some(playlist(PlaylistKind::Manual)),
            ..Row::default()
        };
        let mut page = Page {
            rows: vec![row],
            picker_track_id: Some("track".into()),
            ..Page::default()
        };
        let route = Route::Picker("track".into());
        let (track, target) = add_target(&route, &page, Some(0)).unwrap();
        assert_eq!(
            (track, target.id.as_str(), target.revision),
            ("track", "list", 4)
        );
        assert!(add_target(&Route::Picker("another".into()), &page, Some(0)).is_err());
        page.rows[0].playlist = Some(playlist(PlaylistKind::Derived));
        assert!(add_target(&route, &page, Some(0)).is_err());
    }

    #[test]
    fn likes_resolve_track_not_playlist_entry_id() {
        let page = Page {
            rows: vec![Row {
                id: "track".into(),
                liked: Some(false),
                key: RowKey::ManualEntry {
                    entry_id: "entry".into(),
                    playlist_id: "list".into(),
                },
                ..Row::default()
            }],
            ..Page::default()
        };
        assert_eq!(
            like_target(&Route::Playlist("list".into()), &page, Some(0)).unwrap(),
            ("track", Some(false))
        );
        assert!(like_target(&Route::Playlists, &page, Some(1)).is_err());
    }

    async fn wire_fixture() -> (Access, TcpListener) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = Origin::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let api = Arc::new(Api::new(origin).unwrap());
        let access = Access {
            session: api.test_session(),
            api,
            epoch: 1,
            offline: false,
            recent_change: 0,
        };
        (access, listener)
    }

    // In-process, one-shot HTTP fixture: no live service or UI. Capture the
    // actual request sent by mutate (so a status read cannot pass the Like test).
    async fn capture_write(
        listener: TcpListener,
        response: Value,
        mutation: impl Future<Output = Result<Option<Route>, ApiError>>,
    ) -> (String, Value) {
        let server = async {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut request = String::new();
            stream.read_line(&mut request).await.unwrap();
            let mut length = None;
            loop {
                let mut header = String::new();
                assert_ne!(stream.read_line(&mut header).await.unwrap(), 0);
                if header == "\r\n" {
                    break;
                }
                if let Some((name, value)) = header.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        length = Some(value.trim().parse::<usize>().unwrap());
                    }
                }
            }
            let size = length.unwrap();
            assert!(size < 4096);
            let mut body = vec![0; size];
            stream.read_exact(&mut body).await.unwrap();
            let response = response.to_string();
            stream.get_mut().write_all(format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()
            ).as_bytes()).await.unwrap();
            (request, serde_json::from_slice(&body).unwrap())
        };
        let (captured, result) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(server, mutation)
        })
        .await
        .unwrap();
        assert_eq!(result, Ok(None));
        captured
    }

    #[tokio::test]
    async fn description_only_patch_preserves_the_full_eighty_emoji_name() {
        let (access, listener) = wire_fixture().await;
        let name = "🎵".repeat(80);
        let playlist = PlaylistMetadata {
            name: name.clone(),
            ..playlist(PlaylistKind::Manual)
        };
        let route = Route::Playlist("list".into());
        let page = Page {
            title: "🎵".repeat(64), // 256-byte presentation limit, not the editable name.
            description: "Previous description".into(),
            playlist: Some(playlist),
            ..Page::default()
        };
        let mut draft = prepare(&route, &page, "rename").unwrap();
        assert_eq!(draft.name, name);
        draft.description = "Changed description only".into();
        let envelope = json!({"name": draft.name, "description": draft.description, "definition": draft.definition}).to_string();
        let summary = json!({
            "id": "list", "name": name, "description": "Changed description only",
            "kind": "manual", "countState": "exact", "trackCount": 2, "availableCount": 2,
            "revision": 5, "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:01Z",
            "capabilities": {"editMetadata": true, "editEntries": true, "editDefinition": false,
                "play": true, "copyToManual": false, "delete": true, "addTracks": true}
        });
        let (request, body) = capture_write(
            listener,
            summary,
            mutate(&access, &route, &page, "rename", None, &envelope),
        )
        .await;
        assert_eq!(request, "PATCH /api/me/playlists/list HTTP/1.1\r\n");
        assert_eq!(
            body,
            json!({"name": name, "description": "Changed description only", "expectedRevision": 4})
        );
    }

    #[tokio::test]
    async fn like_put_sets_true_for_confirmed_and_unknown_states_and_unlike_sets_false() {
        let route = Route::Track("track".into());
        for (known, operation, liked) in [
            (Some(true), "like", true),
            (None, "like", true),
            (Some(true), "unlike", false),
        ] {
            let page = liked_state(known);
            let (access, listener) = wire_fixture().await;
            let expected = json!({"trackId": "track", "liked": liked});
            let (request, body) = capture_write(
                listener,
                expected.clone(),
                mutate(&access, &route, &page, operation, Some(0), ""),
            )
            .await;
            assert_eq!(request, "PUT /api/me/likes HTTP/1.1\r\n");
            assert_eq!(body, expected);
        }
    }

    fn liked_state(liked: Option<bool>) -> Page {
        Page {
            rows: vec![Row {
                id: "track".into(),
                key: RowKey::Track("track".into()),
                liked,
                ..Row::default()
            }],
            ..Page::default()
        }
    }

    #[test]
    fn unknown_like_is_set_true_without_enabling_unlike() {
        let route = Route::Track("track".into());
        let page = liked_state(None);
        assert_eq!(
            like_change(&route, &page, Some(0), true).unwrap(),
            ("track", true)
        );
        assert!(like_change(&route, &page, Some(0), false).is_err());
    }
}
