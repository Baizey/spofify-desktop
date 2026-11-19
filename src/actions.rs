//! Playlist forms submit a JSON envelope with `name`, `description`, and a
//! `definition` string. The definition is human-readable, one directive per line;
//! it is not raw JSON. `prepare` returns the same fields for prefilling the form.
//!
//! Example:
//! ```text
//! Source: Saved library
//! Source: Liked tracks
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
        library::{
            PlaylistKind, RuleAvailability, RuleDefinition, RuleDuplicates, RuleMembership,
            RulePredicate, RuleSide, RuleSort, RuleSource, RuleTake,
        },
    },
    pages::{Page, PlaylistMetadata, Route, Row, RowKey},
    personal::PersonalCommand,
    personal_bridge::{Effect, Mutation},
};

pub const DEFAULT_RULES: &str =
    "Source: Saved library\nSort: Source order\nDuplicates: Keep one copy";
pub const RULES_HELP: &str = "Use one rule per line. Try ‘Source: Saved library’ or ‘Source: Playlist’ followed by its ID. Optional: ‘Sort: Title’, ‘Duplicates: Keep one copy’, and ‘Limit: First 50’. Add ‘Include:’ on its own line, then ‘  Genre: jazz’ (two spaces). Use ‘Exclude:’ to leave matches out.";

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
                "Add at least one source, for example ‘Source: Saved library’.",
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

/// Prefill from the retained page so the form and its local edit basis describe
/// the same snapshot. No reads are needed to open a form.
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
        "copy" => {
            let playlist = copy_source(route, page)?;
            const SUFFIX: &str = " (copy)";
            let name: String = playlist.name.chars().take(100 - SUFFIX.len()).collect();
            Ok(Draft {
                name: format!("{name}{SUFFIX}"),
                description: page.description.clone(),
                ..Draft::default()
            })
        }
        _ => Err(error("This playlist form is not available.")),
    }
}

/// Translate one explicit intent against the retained page. Revisions are local
/// observation baselines only: Go resolves the store fence in its account lane.
/// Submission, offline/account fencing and observing matching state are separate.
pub fn mutation(
    route: &Route,
    page: &Page,
    op: &str,
    index: Option<usize>,
    value: &str,
) -> Result<Mutation, ApiError> {
    let (command, effect) = match op {
        "new-manual" | "new-rules" => {
            let draft = read_draft(value)?;
            let (name, description) = metadata(&draft)?;
            let rules = op == "new-rules";
            let track_id = if matches!(route, Route::Picker(_)) {
                require(
                    !rules,
                    "Only manual playlists can accept individual tracks.",
                )?;
                Some(picker_track(route, page)?.to_owned())
            } else {
                None
            };
            let definition = rules
                .then(|| RecipeParser::new(&draft.definition)?.parse(None))
                .transpose()?;
            let effect = Effect::Create {
                name: name.into(),
                description: description.into(),
                kind: if rules {
                    PlaylistKind::Derived
                } else {
                    PlaylistKind::Manual
                },
                has_track: track_id.is_some(),
            };
            (
                PersonalCommand::CreatePlaylist {
                    name: name.into(),
                    description: description.into(),
                    definition,
                    track_id,
                },
                effect,
            )
        }
        "rename" => {
            let playlist = current_playlist(route, page)?;
            require(
                playlist.capabilities.edit_metadata,
                "You can’t change this playlist’s details.",
            )?;
            let draft = read_draft(value)?;
            let (name, description) = metadata(&draft)?;
            (
                PersonalCommand::RenamePlaylist {
                    playlist_id: playlist.id.clone(),
                    name: name.into(),
                    description: description.into(),
                },
                Effect::Rename {
                    id: playlist.id.clone(),
                    name: name.into(),
                    description: description.into(),
                    revision: playlist.revision,
                },
            )
        }
        "rules" => {
            let playlist = current_playlist(route, page)?;
            let previous = current_definition(route, page)?;
            let draft = read_draft(value)?;
            let definition = RecipeParser::new(&draft.definition)?.parse(Some(previous))?;
            (
                PersonalCommand::SetPlaylistRules {
                    playlist_id: playlist.id.clone(),
                    definition: definition.clone(),
                },
                Effect::Rules {
                    id: playlist.id.clone(),
                    definition,
                    revision: playlist.revision,
                },
            )
        }
        "copy" => {
            let playlist = copy_source(route, page)?;
            let draft = read_draft(value)?;
            let (name, description) = metadata(&draft)?;
            (
                PersonalCommand::CopyPlaylist {
                    playlist_id: playlist.id.clone(),
                    name: name.into(),
                    description: description.into(),
                },
                Effect::Create {
                    name: name.into(),
                    description: description.into(),
                    kind: PlaylistKind::Manual,
                    has_track: false,
                },
            )
        }
        "delete" => {
            // The UI owns confirmation. Only a playlist detail can be deleted.
            let playlist = current_playlist(route, page)?;
            require(
                playlist.capabilities.delete,
                "You can’t delete this playlist.",
            )?;
            (
                PersonalCommand::DeletePlaylist {
                    playlist_id: playlist.id.clone(),
                },
                Effect::Delete {
                    id: playlist.id.clone(),
                },
            )
        }
        "add" => {
            let (track, playlist) = add_target(route, page, index)?;
            (
                PersonalCommand::AddEntry {
                    playlist_id: playlist.id.clone(),
                    track_id: track.into(),
                },
                Effect::Add {
                    id: playlist.id.clone(),
                    track_id: track.into(),
                    revision: playlist.revision,
                    visit: true,
                },
            )
        }
        "remove" | "up" | "down" => {
            let playlist = current_playlist(route, page)?;
            require(
                playlist.kind == PlaylistKind::Manual && playlist.capabilities.edit_entries,
                "Only tracks in a manual playlist can be removed or moved.",
            )?;
            let (index, row) = selected(page, index)?;
            let entry = entry_id(row, &playlist.id)?;
            if op == "remove" {
                (
                    PersonalCommand::RemoveEntry {
                        playlist_id: playlist.id.clone(),
                        entry_id: entry.into(),
                    },
                    Effect::Remove {
                        id: playlist.id.clone(),
                        entry_id: entry.into(),
                        revision: playlist.revision,
                    },
                )
            } else {
                let after = move_anchor(page, &playlist.id, index, entry, op == "up")?;
                (
                    PersonalCommand::MoveEntry {
                        playlist_id: playlist.id.clone(),
                        entry_id: entry.into(),
                        after: after.into(),
                    },
                    Effect::Move {
                        id: playlist.id.clone(),
                        entry_id: entry.into(),
                        after: after.into(),
                        revision: playlist.revision,
                    },
                )
            }
        }
        "clear-history" if matches!(route, Route::History) => {
            (PersonalCommand::ClearHistory, Effect::ClearHistory)
        }
        _ => return Err(error("That library action is not available here.")),
    };
    require(
        command.valid(),
        "Review the playlist or track before trying again.",
    )?;
    Ok(Mutation { command, effect })
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

fn copy_source<'a>(route: &Route, page: &'a Page) -> Result<&'a PlaylistMetadata, ApiError> {
    let playlist = current_playlist(route, page)?;
    require(
        playlist.kind == PlaylistKind::Derived && playlist.capabilities.copy_to_manual,
        "This playlist can’t be copied to a manual playlist.",
    )?;
    Ok(playlist)
}

fn selected(page: &Page, index: Option<usize>) -> Result<(usize, &Row), ApiError> {
    index
        .and_then(|index| page.rows.get(index).map(|row| (index, row)))
        .ok_or(error("Select an item first."))
}

fn picker_track<'a>(route: &'a Route, page: &Page) -> Result<&'a str, ApiError> {
    let Route::Picker(track) = route else {
        return Err(error("Choose a playlist to add the track to."));
    };
    if track.is_empty() || page.picker_track_id.as_ref() != Some(track) {
        return Err(error("Select the track again before adding it."));
    }
    Ok(track)
}

fn add_target<'a>(
    route: &'a Route,
    page: &'a Page,
    index: Option<usize>,
) -> Result<(&'a str, &'a PlaylistMetadata), ApiError> {
    let track = picker_track(route, page)?;
    let (_, row) = selected(page, index)?;
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

/// One v3 move places the selected occurrence *after* its new predecessor.
/// Moving up skips the previous row; moving down uses the next row as the anchor.
fn move_anchor<'a>(
    page: &'a Page,
    playlist_id: &str,
    index: usize,
    entry: &str,
    up: bool,
) -> Result<&'a str, ApiError> {
    let anchor = if up {
        if index == 0 {
            return Err(error("This track is already first in the playlist."));
        }
        let preceding = entry_id(&page.rows[index - 1], playlist_id)?;
        if preceding == entry {
            return Err(STALE_VIEW);
        }
        if index == 1 {
            return Ok(""); // An explicit empty anchor is required by the wire protocol.
        }
        let anchor = entry_id(&page.rows[index - 2], playlist_id)?;
        if anchor == preceding {
            return Err(STALE_VIEW);
        }
        anchor
    } else {
        let next = page.rows.get(index + 1).ok_or_else(|| {
            if page.more {
                error("Load more tracks before moving this one down.")
            } else {
                error("This track is already last in the playlist.")
            }
        })?;
        entry_id(next, playlist_id)?
    };
    if anchor == entry {
        return Err(STALE_VIEW);
    }
    Ok(anchor)
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
    use crate::api::library::PlaylistCapabilities;
    use serde_json::{Value, json};

    fn wire(mutation: &Mutation) -> Value {
        serde_json::to_value(&mutation.command).unwrap()
    }

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
        assert_eq!(wire["sources"][0]["kind"], "allSaved");
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
        assert!(matches!(
            parse(DEFAULT_RULES).unwrap().sources.as_slice(),
            [RuleSource::AllSaved { .. }]
        ));
    }

    #[test]
    fn manual_commands_target_occurrences_even_when_tracks_repeat() {
        let row = |id: &str| Row {
            id: "same-track".into(),
            key: RowKey::ManualEntry {
                playlist_id: "list".into(),
                entry_id: id.into(),
            },
            ..Row::default()
        };
        let route = Route::Playlist("list".into());
        let mut page = Page {
            rows: vec![row("entry-1"), row("entry-2"), row("entry-3")],
            playlist: Some(playlist(PlaylistKind::Manual)),
            more: true,
            ..Page::default()
        };
        for (op, index, entry, after) in [
            ("up", 1, "entry-2", ""),
            ("up", 2, "entry-3", "entry-1"),
            ("down", 1, "entry-2", "entry-3"),
        ] {
            let edit = mutation(&route, &page, op, Some(index), "").unwrap();
            assert_eq!(
                wire(&edit),
                json!({
                    "type": "moveEntry", "playlistId": "list", "entryId": entry, "after": after
                })
            );
            assert!(
                matches!(edit.effect, Effect::Move { id, entry_id, after: anchor, revision: 4 }
                if id == "list" && entry_id == entry && anchor == after)
            );
        }
        assert!(mutation(&route, &page, "up", Some(0), "").is_err());
        assert!(mutation(&route, &page, "down", Some(2), "").is_err());
        let removal = mutation(&route, &page, "remove", Some(1), "").unwrap();
        assert_eq!(
            wire(&removal),
            json!({"type":"removeEntry", "playlistId":"list", "entryId":"entry-2"})
        );
        page.rows[1].key = RowKey::ManualEntry {
            playlist_id: "another".into(),
            entry_id: "entry-2".into(),
        };
        assert!(mutation(&route, &page, "remove", Some(1), "").is_err());
        assert!(mutation(&route, &page, "down", Some(0), "").is_err());
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
        let add = mutation(&route, &page, "add", Some(0), "").unwrap();
        assert_eq!(
            wire(&add),
            json!({"type":"addEntry", "playlistId":"list", "trackId":"track"})
        );
        assert!(
            matches!(add.effect, Effect::Add { id, track_id, revision: 4, visit: true }
            if id == "list" && track_id == "track")
        );
        assert!(mutation(&Route::Picker("another".into()), &page, "add", Some(0), "").is_err());
        page.rows[0].playlist = Some(playlist(PlaylistKind::Derived));
        assert!(mutation(&route, &page, "add", Some(0), "").is_err());
    }

    #[test]
    fn description_only_edit_preserves_the_full_eighty_emoji_name() {
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
        let edit = mutation(&route, &page, "rename", None, &envelope).unwrap();
        assert_eq!(
            wire(&edit),
            json!({"type":"renamePlaylist", "playlistId":"list", "name":name, "description":"Changed description only"})
        );
        assert!(matches!(edit.effect, Effect::Rename { revision: 4, .. }));
    }

    #[test]
    fn create_uses_definition_or_picker_track_as_appropriate() {
        let envelope = json!({"name": "  A new list  ", "description": "Description", "definition": DEFAULT_RULES}).to_string();
        let page = Page::default();
        let manual = mutation(&Route::Playlists, &page, "new-manual", None, &envelope).unwrap();
        let metadata =
            json!({"type":"createPlaylist", "name":"A new list", "description":"Description"});
        assert_eq!(wire(&manual), metadata);
        let picker = Route::Picker("track".into());
        let picker_page = Page {
            picker_track_id: Some("track".into()),
            ..Page::default()
        };
        let with_track = mutation(&picker, &picker_page, "new-manual", None, &envelope).unwrap();
        let mut expected = metadata;
        expected["trackId"] = json!("track");
        assert_eq!(wire(&with_track), expected);
        assert!(matches!(
            with_track.effect,
            Effect::Create {
                kind: PlaylistKind::Manual,
                has_track: true,
                ..
            }
        ));
        assert!(mutation(&picker, &picker_page, "new-rules", None, &envelope).is_err());
        let rules = mutation(&Route::Playlists, &page, "new-rules", None, &envelope).unwrap();
        let command = wire(&rules);
        assert_eq!(command["type"], "createPlaylist");
        assert_eq!(command["definition"]["sources"][0]["kind"], "allSaved");
        assert!(command.get("trackId").is_none());
        assert!(matches!(
            rules.effect,
            Effect::Create {
                kind: PlaylistKind::Derived,
                has_track: false,
                ..
            }
        ));
    }

    #[test]
    fn rules_edit_keeps_slots_and_uses_the_v3_definition_command() {
        let previous = parse(DEFAULT_RULES).unwrap();
        let route = Route::Playlist("list".into());
        let page = Page {
            playlist: Some(playlist(PlaylistKind::Derived)),
            definition: Some(previous.clone()),
            ..Page::default()
        };
        let envelope = json!({"definition": "Source: Saved library\nSort: Title"}).to_string();
        let edit = mutation(&route, &page, "rules", None, &envelope).unwrap();
        let expected = RuleDefinition {
            sort: RuleSort::Title,
            ..previous
        };
        assert_eq!(
            wire(&edit),
            json!({"type":"setPlaylistRules", "playlistId":"list", "definition":expected})
        );
        assert!(
            matches!(edit.effect, Effect::Rules { id, definition, revision: 4 }
            if id == "list" && definition == expected)
        );
    }

    #[test]
    fn delete_requires_the_retained_playlist_and_retired_track_actions_stay_unavailable() {
        let page = Page {
            playlist: Some(playlist(PlaylistKind::Manual)),
            ..Page::default()
        };
        let route = Route::Playlist("list".into());
        let deletion = mutation(&route, &page, "delete", None, "").unwrap();
        assert_eq!(
            wire(&deletion),
            json!({"type":"deletePlaylist", "playlistId":"list"})
        );
        assert!(
            mutation(
                &Route::Playlist("another".into()),
                &page,
                "delete",
                None,
                ""
            )
            .is_err()
        );
        let route = Route::Track("track".into());
        for operation in ["like", "unlike"] {
            let error = mutation(&route, &page, operation, Some(0), "")
                .err()
                .unwrap();
            assert_eq!(error.message, "That library action is not available here.");
        }
    }
}
