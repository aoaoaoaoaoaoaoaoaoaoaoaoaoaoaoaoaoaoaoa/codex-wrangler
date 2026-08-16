use std::{cmp::Ordering, collections::HashSet, ops::Range};

use regex::{Regex, RegexBuilder};

use crate::{
    contract::{HistoryColumn, SortDirection},
    history::Session,
    model::Card,
};

#[derive(Debug)]
pub struct Hit {
    card: usize,
    spans: Vec<Range<usize>>,
}

impl Hit {
    pub const fn card(&self) -> usize {
        self.card
    }

    pub fn spans(&self) -> &[Range<usize>] {
        &self.spans
    }
}

pub struct Scry {
    query: String,
    matcher: Option<Regex>,
    valid: bool,
    hits: Vec<Hit>,
    label: String,
}

impl Default for Scry {
    fn default() -> Self {
        Self {
            query: String::new(),
            matcher: None,
            valid: true,
            hits: Vec::new(),
            label: "0 MANUAL THREADS".to_owned(),
        }
    }
}

impl Scry {
    pub fn query(&self) -> &str {
        &self.query
    }

    pub const fn valid(&self) -> bool {
        self.valid
    }

    pub fn edit(&mut self) -> &mut String {
        &mut self.query
    }

    pub fn hits(&self) -> &[Hit] {
        &self.hits
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn revise(&mut self, cards: &[Card]) {
        (self.matcher, self.valid) = compile(&self.query);
        self.reconcile(cards);
    }

    pub fn clear(&mut self, cards: &[Card]) {
        self.query.clear();
        self.matcher = None;
        self.valid = true;
        self.reconcile(cards);
    }

    pub fn reconcile(&mut self, cards: &[Card]) {
        self.hits.clear();
        let Some(matcher) = self.matcher.as_ref() else {
            self.hits.extend((0..cards.len()).map(|card| Hit {
                card,
                spans: Vec::new(),
            }));
            self.label = if self.valid {
                census_label(cards.len())
            } else {
                "INVALID REGEXP".to_owned()
            };
            return;
        };

        for (card, model) in cards.iter().enumerate() {
            let haystack = model
                .name
                .as_deref()
                .filter(|name| !name.is_empty())
                .unwrap_or(&model.cwd);
            let mut findings = matcher.find_iter(haystack).peekable();
            if findings.peek().is_none() {
                continue;
            }
            let spans = findings
                .filter(|found| found.start() != found.end())
                .map(|found| found.start()..found.end())
                .collect();
            self.hits.push(Hit { card, spans });
        }
        self.label = format!("{} OF {} MANUAL THREADS", self.hits.len(), cards.len());
    }
}

fn census_label(count: usize) -> String {
    let noun = if count == 1 { "THREAD" } else { "THREADS" };
    format!("{count} MANUAL {noun}")
}

#[derive(Debug)]
pub struct HistoryHit {
    session: usize,
    id_spans: Vec<Range<usize>>,
    name_spans: Vec<Range<usize>>,
}

impl HistoryHit {
    pub const fn session(&self) -> usize {
        self.session
    }

    pub fn id_spans(&self) -> &[Range<usize>] {
        &self.id_spans
    }

    pub fn name_spans(&self) -> &[Range<usize>] {
        &self.name_spans
    }
}

pub struct HistoryScry {
    query: String,
    matcher: Option<Regex>,
    valid: bool,
    hits: Vec<HistoryHit>,
    sorts: Vec<HistorySort>,
    label: String,
}

#[derive(Clone, Copy)]
struct HistorySort {
    column: HistoryColumn,
    direction: SortDirection,
}

impl Default for HistoryScry {
    fn default() -> Self {
        Self {
            query: String::new(),
            matcher: None,
            valid: true,
            hits: Vec::new(),
            sorts: Vec::new(),
            label: "0 HISTORICAL SESSIONS".to_owned(),
        }
    }
}

impl HistoryScry {
    pub fn query(&self) -> &str {
        &self.query
    }

    pub const fn valid(&self) -> bool {
        self.valid
    }

    pub fn edit(&mut self) -> &mut String {
        &mut self.query
    }

    pub fn hits(&self) -> &[HistoryHit] {
        &self.hits
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn direction(&self, column: HistoryColumn) -> Option<SortDirection> {
        self.sorts
            .iter()
            .find(|sort| sort.column == column)
            .map(|sort| sort.direction)
    }

    #[cfg(feature = "egui-test")]
    pub fn sorts(&self) -> impl Iterator<Item = (HistoryColumn, SortDirection)> + '_ {
        self.sorts.iter().map(|sort| (sort.column, sort.direction))
    }

    pub fn cycle(&mut self, column: HistoryColumn, sessions: &[Session]) {
        let direction = self.direction(column);
        self.sorts.retain(|sort| sort.column != column);
        match direction {
            None => self.sorts.push(HistorySort {
                column,
                direction: SortDirection::Ascending,
            }),
            Some(SortDirection::Ascending) => self.sorts.push(HistorySort {
                column,
                direction: SortDirection::Descending,
            }),
            Some(SortDirection::Descending) => {}
        }
        self.order(sessions);
    }

    pub fn revise(&mut self, sessions: &[Session], live: &HashSet<String>) {
        (self.matcher, self.valid) = compile(&self.query);
        self.reconcile(sessions, live);
    }

    pub fn clear(&mut self, sessions: &[Session], live: &HashSet<String>) {
        self.query.clear();
        self.matcher = None;
        self.valid = true;
        self.reconcile(sessions, live);
    }

    pub fn reconcile(&mut self, sessions: &[Session], live: &HashSet<String>) {
        self.hits.clear();
        let total = sessions
            .iter()
            .filter(|session| !live.contains(session.thread.as_str()))
            .count();
        let Some(matcher) = self.matcher.as_ref() else {
            self.hits.extend(
                sessions
                    .iter()
                    .enumerate()
                    .filter(|(_, session)| !live.contains(session.thread.as_str()))
                    .map(|(session, _)| HistoryHit {
                        session,
                        id_spans: Vec::new(),
                        name_spans: Vec::new(),
                    }),
            );
            self.label = if self.valid {
                history_label(total)
            } else {
                "INVALID REGEXP".to_owned()
            };
            self.order(sessions);
            return;
        };

        for (index, session) in sessions.iter().enumerate() {
            if live.contains(session.thread.as_str()) {
                continue;
            }
            let id_match = matcher.is_match(&session.thread);
            let name_match = session
                .name
                .as_deref()
                .is_some_and(|name| matcher.is_match(name));
            if !id_match && !name_match {
                continue;
            }
            let id_spans = spans(matcher, &session.thread);
            let name_spans = session
                .name
                .as_deref()
                .map_or_else(Vec::new, |name| spans(matcher, name));
            self.hits.push(HistoryHit {
                session: index,
                id_spans,
                name_spans,
            });
        }
        self.label = format!("{} OF {} HISTORICAL SESSIONS", self.hits.len(), total);
        self.order(sessions);
    }

    fn order(&mut self, sessions: &[Session]) {
        self.hits.sort_by_key(HistoryHit::session);
        for sort in &self.sorts {
            self.hits.sort_by(|left, right| {
                compare(&sessions[left.session], &sessions[right.session], *sort)
            });
        }
    }
}

fn spans(matcher: &Regex, text: &str) -> Vec<Range<usize>> {
    matcher
        .find_iter(text)
        .filter(|found| found.start() != found.end())
        .map(|found| found.start()..found.end())
        .collect()
}

fn history_label(count: usize) -> String {
    let noun = if count == 1 { "SESSION" } else { "SESSIONS" };
    format!("{count} HISTORICAL {noun}")
}

fn compile(query: &str) -> (Option<Regex>, bool) {
    if query.is_empty() {
        return (None, true);
    }
    match RegexBuilder::new(query).case_insensitive(true).build() {
        Ok(matcher) => (Some(matcher), true),
        Err(_) => (None, false),
    }
}

fn compare(left: &Session, right: &Session, sort: HistorySort) -> Ordering {
    let order = match sort.column {
        HistoryColumn::SessionId => left.thread.cmp(&right.thread),
        HistoryColumn::Name => {
            return optional(
                left.name.as_deref(),
                right.name.as_deref(),
                sort.direction,
                folded,
            );
        }
        HistoryColumn::LastTurn => left.updated_at_ms.cmp(&right.updated_at_ms),
        HistoryColumn::Turns => {
            return optional(left.turns, right.turns, sort.direction, Ord::cmp);
        }
        HistoryColumn::Size => left.bytes.cmp(&right.bytes),
        HistoryColumn::State => left.archived.cmp(&right.archived),
    };
    directed(order, sort.direction)
}

fn optional<T>(
    left: Option<T>,
    right: Option<T>,
    direction: SortDirection,
    compare: impl FnOnce(&T, &T) -> Ordering,
) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => directed(compare(&left, &right), direction),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn directed(order: Ordering, direction: SortDirection) -> Ordering {
    match direction {
        SortDirection::Ascending => order,
        SortDirection::Descending => order.reverse(),
    }
}

fn folded(left: &&str, right: &&str) -> Ordering {
    left.bytes()
        .map(|byte| byte.to_ascii_lowercase())
        .cmp(right.bytes().map(|byte| byte.to_ascii_lowercase()))
}
