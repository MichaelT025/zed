use std::sync::Arc;

use collections::{HashMap, HashSet};
use git::{Oid, repository::InitialGraphCommitData};
use gpui::{Hsla, PathBuilder, Pixels, Point, SharedString, Window, point, px, rgb};
use smallvec::SmallVec;

pub(crate) const ROW_HEIGHT: Pixels = px(22.);
pub(crate) const LANE_WIDTH: Pixels = px(11.);
const CURVE_RADIUS: Pixels = px(5.);
const CIRCLE_RADIUS: Pixels = px(4.);
const CIRCLE_STROKE_WIDTH: Pixels = px(2.);
pub(crate) const PALETTE_SIZE: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum GraphNodeId {
    Commit(Oid),
    IncomingChanges,
    OutgoingChanges,
}

/// Local/Remote/Base are VS Code's fixed ref colors; Palette(n) cycles 0..PALETTE_SIZE.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LaneColor {
    Local,
    Remote,
    Base,
    Palette(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Swimlane {
    pub id: GraphNodeId,
    pub color: LaneColor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GraphRowKind {
    Head,
    Node,
    IncomingChanges,
    OutgoingChanges,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RefKind {
    LocalBranch,
    RemoteBranch,
    Tag,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RefBadge {
    pub name: SharedString,
    pub kind: RefKind,
    pub color: LaneColor,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GraphRow {
    pub id: GraphNodeId,
    pub kind: GraphRowKind,
    pub parents: SmallVec<[GraphNodeId; 2]>,
    pub input_swimlanes: Vec<Swimlane>,
    pub output_swimlanes: Vec<Swimlane>,
    /// Sorted: current, remote, base, then the rest (stable).
    pub badges: Vec<RefBadge>,
}

/// Which refs are emphasized. Names are short names as they appear in `%D`
/// after normalization (e.g. "main", "origin/main").
#[derive(Clone, Debug, Default)]
pub(crate) struct GraphRefs {
    pub head_sha: Option<Oid>,
    pub current: Option<SharedString>,
    pub remote: Option<SharedString>,
    pub base: Option<SharedString>,
    pub remote_names: Vec<SharedString>,
    pub badges: BadgeFilter,
    pub show_incoming_changes: bool,
    pub show_outgoing_changes: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum BadgeFilter {
    /// Only current/remote/base get badges (VS Code "auto").
    #[default]
    Emphasized,
    /// Emphasized refs plus these names, colored by their lane.
    Listed(Vec<SharedString>),
    /// Every branch and tag gets a badge, colored by its lane.
    All,
}

type ColorMap = HashMap<SharedString, Option<LaneColor>>;
type NormalizedRef = (SharedString, RefKind);

pub(crate) fn layout_graph(
    commits: &[Arc<InitialGraphCommitData>],
    refs: &GraphRefs,
) -> Vec<GraphRow> {
    let normalized: Vec<Vec<NormalizedRef>> = commits
        .iter()
        .map(|commit| normalize_commit(commit, &refs.remote_names))
        .collect();
    let color_map = build_color_map(&normalized, refs);
    let index_by_sha: HashMap<Oid, usize> = commits
        .iter()
        .enumerate()
        .map(|(index, commit)| (commit.sha, index))
        .collect();

    let mut palette_cursor = 0usize;
    let mut rows: Vec<GraphRow> = Vec::with_capacity(commits.len());

    for (commit_index, commit) in commits.iter().enumerate() {
        let row_id = GraphNodeId::Commit(commit.sha);
        let kind = if refs.head_sha == Some(commit.sha) {
            GraphRowKind::Head
        } else {
            GraphRowKind::Node
        };
        let row_label_color = normalized
            .get(commit_index)
            .and_then(|commit_refs| label_color(commit_refs, &color_map));

        let input_swimlanes = rows
            .last()
            .map(|row| row.output_swimlanes.clone())
            .unwrap_or_default();
        let mut output_swimlanes = Vec::new();
        let mut first_parent_added = false;

        if let Some(first_parent) = commit.parents.first().copied() {
            for lane in &input_swimlanes {
                if lane.id == row_id {
                    if !first_parent_added {
                        output_swimlanes.push(Swimlane {
                            id: GraphNodeId::Commit(first_parent),
                            color: row_label_color.unwrap_or(lane.color),
                        });
                        first_parent_added = true;
                    }

                    continue;
                }

                output_swimlanes.push(*lane);
            }
        }

        let first_unprocessed_parent = if first_parent_added { 1 } else { 0 };
        for (parent_index, parent) in commit
            .parents
            .iter()
            .enumerate()
            .skip(first_unprocessed_parent)
        {
            let color = if parent_index == 0 {
                row_label_color
            } else {
                index_by_sha
                    .get(parent)
                    .and_then(|parent_index| normalized.get(*parent_index))
                    .and_then(|parent_refs| label_color(parent_refs, &color_map))
            };
            let color = match color {
                Some(color) => color,
                None => {
                    let color = LaneColor::Palette(palette_cursor);
                    palette_cursor = (palette_cursor + 1) % PALETTE_SIZE;
                    color
                }
            };

            output_swimlanes.push(Swimlane {
                id: GraphNodeId::Commit(*parent),
                color,
            });
        }

        let circle_index = input_swimlanes
            .iter()
            .position(|lane| lane.id == row_id)
            .unwrap_or(input_swimlanes.len());
        let circle_color = output_swimlanes
            .get(circle_index)
            .map(|lane| lane.color)
            .or_else(|| input_swimlanes.get(circle_index).map(|lane| lane.color))
            .unwrap_or(LaneColor::Local);

        let mut badges = Vec::new();
        if let Some(commit_refs) = normalized.get(commit_index) {
            for (name, kind) in commit_refs {
                let Some(entry) = color_map.get(name.as_ref()) else {
                    continue;
                };
                badges.push(RefBadge {
                    name: name.clone(),
                    kind: *kind,
                    color: (*entry).unwrap_or(circle_color),
                });
            }
        }
        badges.sort_by_key(|badge| badge_order(badge, refs));

        rows.push(GraphRow {
            id: row_id,
            kind,
            parents: commit
                .parents
                .iter()
                .copied()
                .map(GraphNodeId::Commit)
                .collect(),
            input_swimlanes,
            output_swimlanes,
            badges,
        });
    }

    add_incoming_outgoing_changes(&mut rows, commits, &index_by_sha, &normalized, refs);

    rows
}

fn badge_order(badge: &RefBadge, refs: &GraphRefs) -> u8 {
    if refs.current.as_deref() == Some(badge.name.as_ref()) {
        1
    } else if refs.remote.as_deref() == Some(badge.name.as_ref()) {
        2
    } else if refs.base.as_deref() == Some(badge.name.as_ref()) {
        3
    } else {
        4
    }
}

fn add_incoming_outgoing_changes(
    rows: &mut Vec<GraphRow>,
    commits: &[Arc<InitialGraphCommitData>],
    index_by_sha: &HashMap<Oid, usize>,
    normalized: &[Vec<NormalizedRef>],
    refs: &GraphRefs,
) {
    let Some(head) = refs.head_sha else {
        return;
    };
    let Some(remote_name) = refs.remote.as_ref() else {
        return;
    };
    let Some(remote_revision) = commits
        .iter()
        .zip(normalized.iter())
        .find(|(_, commit_refs)| commit_refs.iter().any(|(name, _)| name == remote_name))
        .map(|(commit, _)| commit.sha)
    else {
        return;
    };

    if head == remote_revision {
        return;
    }
    let Some(merge_base) = merge_base(commits, head, remote_revision) else {
        return;
    };

    if refs.show_incoming_changes && remote_revision != merge_base {
        let before_index = rows.iter().rposition(|row| {
            row.output_swimlanes
                .iter()
                .any(|lane| lane.id == GraphNodeId::Commit(merge_base))
        });
        let after_index = rows
            .iter()
            .position(|row| row.id == GraphNodeId::Commit(merge_base));

        if let (Some(before_index), Some(after_index)) = (before_index, after_index) {
            let before_commit = rows.get(before_index).and_then(|row| match row.id {
                GraphNodeId::Commit(sha) => {
                    index_by_sha.get(&sha).and_then(|index| commits.get(*index))
                }
                GraphNodeId::IncomingChanges | GraphNodeId::OutgoingChanges => None,
            });
            let incoming_change_merged = before_commit
                .map(|commit| commit.parents.len() == 2 && commit.parents.contains(&merge_base))
                .unwrap_or(false);

            if !incoming_change_merged {
                if let Some(before_row) = rows.get_mut(before_index) {
                    for lane in &mut before_row.input_swimlanes {
                        if lane.id == GraphNodeId::Commit(merge_base)
                            && lane.color == LaneColor::Remote
                        {
                            lane.id = GraphNodeId::IncomingChanges;
                        }
                    }
                    for lane in &mut before_row.output_swimlanes {
                        if lane.id == GraphNodeId::Commit(merge_base)
                            && lane.color == LaneColor::Remote
                        {
                            lane.id = GraphNodeId::IncomingChanges;
                        }
                    }
                }

                let input_swimlanes = rows
                    .get(before_index)
                    .map(|row| row.output_swimlanes.clone())
                    .unwrap_or_default();
                let output_swimlanes = rows
                    .get(after_index)
                    .map(|row| row.input_swimlanes.clone())
                    .unwrap_or_default();

                rows.insert(
                    after_index,
                    GraphRow {
                        id: GraphNodeId::IncomingChanges,
                        kind: GraphRowKind::IncomingChanges,
                        parents: SmallVec::from_slice(&[GraphNodeId::Commit(merge_base)]),
                        input_swimlanes,
                        output_swimlanes,
                        badges: Vec::new(),
                    },
                );
            }
        }
    }

    if refs.show_outgoing_changes && head != merge_base {
        let head_index = rows
            .iter()
            .position(|row| row.kind == GraphRowKind::Head && row.id == GraphNodeId::Commit(head));

        if let Some(head_index) = head_index {
            let input_swimlanes = rows
                .get(head_index)
                .map(|row| row.input_swimlanes.clone())
                .unwrap_or_default();
            let mut output_swimlanes = input_swimlanes.clone();
            output_swimlanes.push(Swimlane {
                id: GraphNodeId::Commit(head),
                color: LaneColor::Local,
            });

            rows.insert(
                head_index,
                GraphRow {
                    id: GraphNodeId::OutgoingChanges,
                    kind: GraphRowKind::OutgoingChanges,
                    parents: SmallVec::from_slice(&[GraphNodeId::Commit(head)]),
                    input_swimlanes,
                    output_swimlanes,
                    badges: Vec::new(),
                },
            );

            if let Some(head_row) = rows.get_mut(head_index + 1) {
                head_row.input_swimlanes.push(Swimlane {
                    id: GraphNodeId::Commit(head),
                    color: LaneColor::Local,
                });
            }
        }
    }
}

pub(crate) fn merge_base(commits: &[Arc<InitialGraphCommitData>], a: Oid, b: Oid) -> Option<Oid> {
    let index_by_sha: HashMap<Oid, usize> = commits
        .iter()
        .enumerate()
        .map(|(index, commit)| (commit.sha, index))
        .collect();
    let a_ancestors = ancestor_indices(commits, &index_by_sha, a);
    let b_ancestors = ancestor_indices(commits, &index_by_sha, b);

    a_ancestors
        .iter()
        .filter(|index| b_ancestors.contains(index))
        .min()
        .copied()
        .and_then(|index| commits.get(index))
        .map(|commit| commit.sha)
}

fn ancestor_indices(
    commits: &[Arc<InitialGraphCommitData>],
    index_by_sha: &HashMap<Oid, usize>,
    start: Oid,
) -> HashSet<usize> {
    let mut visited = HashSet::default();
    let mut pending = Vec::new();
    if let Some(index) = index_by_sha.get(&start) {
        pending.push(*index);
    }

    while let Some(index) = pending.pop() {
        if !visited.insert(index) {
            continue;
        }
        if let Some(commit) = commits.get(index) {
            for parent in &commit.parents {
                if let Some(parent_index) = index_by_sha.get(parent) {
                    pending.push(*parent_index);
                }
            }
        }
    }

    visited
}

fn normalize_commit(
    commit: &InitialGraphCommitData,
    remote_names: &[SharedString],
) -> Vec<NormalizedRef> {
    commit
        .ref_names
        .iter()
        .filter_map(|raw| normalize_ref(raw, remote_names))
        .collect()
}

fn normalize_ref(raw: &str, remote_names: &[SharedString]) -> Option<NormalizedRef> {
    let raw = raw.trim();
    if raw.is_empty() || raw == "HEAD" {
        return None;
    }
    if let Some(name) = raw.strip_prefix("HEAD -> ") {
        return Some((SharedString::new(name), RefKind::LocalBranch));
    }
    if let Some(name) = raw.strip_prefix("tag: ") {
        return Some((SharedString::new(name), RefKind::Tag));
    }
    if let Some((remote, rest)) = raw.split_once('/')
        && remote_names.iter().any(|known| known.as_ref() == remote)
    {
        if rest == "HEAD" {
            return None;
        }
        return Some((SharedString::new(raw), RefKind::RemoteBranch));
    }

    Some((SharedString::new(raw), RefKind::LocalBranch))
}

fn build_color_map(normalized: &[Vec<NormalizedRef>], refs: &GraphRefs) -> ColorMap {
    let mut color_map = ColorMap::default();
    if let Some(current) = &refs.current {
        color_map.insert(current.clone(), Some(LaneColor::Local));
    }
    if let Some(remote) = &refs.remote {
        color_map.insert(remote.clone(), Some(LaneColor::Remote));
    }
    if let Some(base) = &refs.base {
        color_map.insert(base.clone(), Some(LaneColor::Base));
    }

    match &refs.badges {
        BadgeFilter::Emphasized => {}
        BadgeFilter::Listed(names) => {
            for name in names {
                color_map.entry(name.clone()).or_insert(None);
            }
        }
        BadgeFilter::All => {
            for commit_refs in normalized {
                for (name, _) in commit_refs {
                    color_map.entry(name.clone()).or_insert(None);
                }
            }
        }
    }

    color_map
}

fn label_color(commit_refs: &[NormalizedRef], color_map: &ColorMap) -> Option<LaneColor> {
    commit_refs
        .iter()
        .find_map(|(name, _)| match color_map.get(name.as_ref()) {
            Some(Some(color)) => Some(*color),
            _ => None,
        })
}

/// Width of the graph column for this row: LANE_WIDTH * (max(input, output, 1) + 1).
pub(crate) fn graph_width(row: &GraphRow) -> Pixels {
    let lane_count = row
        .input_swimlanes
        .len()
        .max(row.output_swimlanes.len())
        .max(1);

    LANE_WIDTH * (lane_count + 1)
}

/// Theme-independent graph colors (VS Code defaults).
#[derive(Clone, Copy, Debug)]
pub(crate) struct GraphColors {
    pub local: Hsla,
    pub remote: Hsla,
    pub base: Hsla,
    pub palette: [Hsla; PALETTE_SIZE],
}

impl GraphColors {
    pub(crate) fn vscode_default() -> Self {
        Self {
            local: rgb(0x3794FF).into(),
            remote: rgb(0xB180D7).into(),
            base: rgb(0xEA5C00).into(),
            palette: [
                rgb(0xFFB000).into(),
                rgb(0xDC267F).into(),
                rgb(0x994F00).into(),
                rgb(0x40B0A6).into(),
                rgb(0xB66DFF).into(),
            ],
        }
    }

    pub(crate) fn lane(&self, color: LaneColor) -> Hsla {
        match color {
            LaneColor::Local => self.local,
            LaneColor::Remote => self.remote,
            LaneColor::Base => self.base,
            LaneColor::Palette(index) => self
                .palette
                .get(index % PALETTE_SIZE)
                .copied()
                .unwrap_or(self.local),
        }
    }
}

fn lane_center(index: usize) -> Pixels {
    LANE_WIDTH * (index + 1)
}

fn graph_point(origin: Point<Pixels>, x: Pixels, y: Pixels) -> Point<Pixels> {
    origin + point(x, y)
}

fn paint_stroke(builder: PathBuilder, color: Hsla, window: &mut Window) {
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

fn paint_disc(center: Point<Pixels>, radius: Pixels, color: Hsla, window: &mut Window) {
    if radius <= px(0.) {
        return;
    }

    let mut builder = PathBuilder::fill();
    builder.move_to(center + point(radius, px(0.)));
    builder.arc_to(
        point(radius, radius),
        px(0.),
        false,
        true,
        center + point(-radius, px(0.)),
    );
    builder.arc_to(
        point(radius, radius),
        px(0.),
        false,
        true,
        center + point(radius, px(0.)),
    );
    builder.close();

    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

/// Emulates an SVG circle with a background-colored stroke as two filled discs.
fn paint_circle(
    center: Point<Pixels>,
    radius: Pixels,
    stroke_width: Pixels,
    fill: Hsla,
    background: Hsla,
    window: &mut Window,
) {
    paint_disc(center, radius + stroke_width * 0.5, background, window);
    paint_disc(center, radius - stroke_width * 0.5, fill, window);
}

fn paint_dashed_circle(
    center: Point<Pixels>,
    radius: Pixels,
    stroke_width: Pixels,
    color: Hsla,
    window: &mut Window,
) {
    const DASH_LENGTH: f32 = 4.;
    const GAP_LENGTH: f32 = 2.;
    const SEGMENTS_PER_DASH: usize = 4;

    if radius <= px(0.) || stroke_width <= px(0.) {
        return;
    }

    // GPUI's `dash_array` samples arcs into non-finite points for small circles, which
    // trips a lyon assertion, so the dashes are built as short polylines instead.
    let radius = radius.as_f32();
    let circumference = std::f32::consts::TAU * radius;
    let dash_count = (circumference / (DASH_LENGTH + GAP_LENGTH)).round().max(1.);
    let period = std::f32::consts::TAU / dash_count;
    let dash_angle = period * DASH_LENGTH / (DASH_LENGTH + GAP_LENGTH);

    let mut builder = PathBuilder::stroke(stroke_width);
    for dash in 0..dash_count as usize {
        let start_angle = period * dash as f32;
        for step in 0..=SEGMENTS_PER_DASH {
            let angle = start_angle + dash_angle * step as f32 / SEGMENTS_PER_DASH as f32;
            let position = center + point(px(radius * angle.cos()), px(radius * angle.sin()));
            if step == 0 {
                builder.move_to(position);
            } else {
                builder.line_to(position);
            }
        }
    }
    paint_stroke(builder, color, window);
}

/// Paints one row of the graph. `origin` is the top-left of the graph column for the
/// row (height ROW_HEIGHT). `background` is the row's current background (it changes
/// on hover/selection) and is used for the circle halos, exactly like VS Code's CSS.
pub(crate) fn paint_graph_row(
    row: &GraphRow,
    origin: Point<Pixels>,
    colors: &GraphColors,
    background: Hsla,
    window: &mut Window,
) {
    let input_swimlanes = &row.input_swimlanes;
    let output_swimlanes = &row.output_swimlanes;
    let input_index = input_swimlanes.iter().position(|lane| lane.id == row.id);
    let circle_index = input_index.unwrap_or(input_swimlanes.len());
    let circle_color = output_swimlanes
        .get(circle_index)
        .map(|lane| lane.color)
        .or_else(|| input_swimlanes.get(circle_index).map(|lane| lane.color))
        .unwrap_or(LaneColor::Local);
    let middle = ROW_HEIGHT * 0.5;

    let mut output_swimlane_index = 0usize;
    for (index, lane) in input_swimlanes.iter().enumerate() {
        let color = colors.lane(lane.color);

        if lane.id == row.id {
            if index != circle_index {
                // Merge into the current circle: draw /
                let x = lane_center(index);
                let mut builder = PathBuilder::stroke(px(1.));
                builder.move_to(graph_point(origin, x, px(0.)));
                builder.arc_to(
                    point(LANE_WIDTH, LANE_WIDTH),
                    px(0.),
                    false,
                    true,
                    graph_point(origin, x - LANE_WIDTH, LANE_WIDTH),
                );
                builder.line_to(graph_point(origin, lane_center(circle_index), LANE_WIDTH));
                paint_stroke(builder, color, window);
            } else {
                output_swimlane_index += 1;
            }
        } else if let Some(output_lane) = output_swimlanes.get(output_swimlane_index) {
            if lane.id == output_lane.id {
                if index == output_swimlane_index {
                    let x = lane_center(index);
                    let mut builder = PathBuilder::stroke(px(1.));
                    builder.move_to(graph_point(origin, x, px(0.)));
                    builder.line_to(graph_point(origin, x, ROW_HEIGHT));
                    paint_stroke(builder, color, window);
                } else {
                    let x = lane_center(index);
                    let target = lane_center(output_swimlane_index);
                    let mut builder = PathBuilder::stroke(px(1.));
                    builder.move_to(graph_point(origin, x, px(0.)));
                    builder.line_to(graph_point(origin, x, px(6.)));
                    builder.arc_to(
                        point(CURVE_RADIUS, CURVE_RADIUS),
                        px(0.),
                        false,
                        true,
                        graph_point(origin, x - CURVE_RADIUS, middle),
                    );
                    builder.line_to(graph_point(origin, target + CURVE_RADIUS, middle));
                    builder.arc_to(
                        point(CURVE_RADIUS, CURVE_RADIUS),
                        px(0.),
                        false,
                        false,
                        graph_point(origin, target, middle + CURVE_RADIUS),
                    );
                    builder.line_to(graph_point(origin, target, ROW_HEIGHT));
                    paint_stroke(builder, color, window);
                }

                output_swimlane_index += 1;
            }
        }
    }

    for parent in row.parents.iter().skip(1) {
        let Some(parent_output_index) =
            output_swimlanes.iter().rposition(|lane| lane.id == *parent)
        else {
            continue;
        };
        let Some(parent_lane) = output_swimlanes.get(parent_output_index) else {
            continue;
        };

        let color = colors.lane(parent_lane.color);
        let x = LANE_WIDTH * parent_output_index;
        let mut builder = PathBuilder::stroke(px(1.));
        builder.move_to(graph_point(origin, x, middle));
        builder.arc_to(
            point(LANE_WIDTH, LANE_WIDTH),
            px(0.),
            false,
            true,
            graph_point(origin, x + LANE_WIDTH, ROW_HEIGHT),
        );
        builder.move_to(graph_point(origin, x, middle));
        builder.line_to(graph_point(origin, lane_center(circle_index), middle));
        paint_stroke(builder, color, window);
    }

    if let Some(input_index) = input_index
        && let Some(input_lane) = input_swimlanes.get(input_index)
    {
        let x = lane_center(circle_index);
        let mut builder = PathBuilder::stroke(px(1.));
        builder.move_to(graph_point(origin, x, px(0.)));
        builder.line_to(graph_point(origin, x, middle));
        paint_stroke(builder, colors.lane(input_lane.color), window);
    }

    if !row.parents.is_empty() {
        let x = lane_center(circle_index);
        let mut builder = PathBuilder::stroke(px(1.));
        builder.move_to(graph_point(origin, x, middle));
        builder.line_to(graph_point(origin, x, ROW_HEIGHT));
        paint_stroke(builder, colors.lane(circle_color), window);
    }

    let center = graph_point(origin, lane_center(circle_index), LANE_WIDTH);
    match row.kind {
        GraphRowKind::Head => {
            paint_circle(
                center,
                CIRCLE_RADIUS + px(3.),
                CIRCLE_STROKE_WIDTH,
                colors.lane(circle_color),
                background,
                window,
            );
            paint_circle(
                center,
                CIRCLE_STROKE_WIDTH,
                CIRCLE_RADIUS,
                background,
                background,
                window,
            );
        }
        GraphRowKind::IncomingChanges | GraphRowKind::OutgoingChanges => {
            paint_circle(
                center,
                CIRCLE_RADIUS + px(3.),
                CIRCLE_STROKE_WIDTH,
                colors.lane(circle_color),
                background,
                window,
            );
            paint_circle(
                center,
                CIRCLE_RADIUS + px(1.),
                CIRCLE_STROKE_WIDTH + px(1.),
                background,
                background,
                window,
            );
            paint_dashed_circle(
                center,
                CIRCLE_RADIUS + px(1.),
                CIRCLE_STROKE_WIDTH - px(1.),
                colors.lane(circle_color),
                window,
            );
        }
        GraphRowKind::Node => {
            if row.parents.len() > 1 {
                paint_circle(
                    center,
                    CIRCLE_RADIUS + px(2.),
                    CIRCLE_STROKE_WIDTH,
                    colors.lane(circle_color),
                    background,
                    window,
                );
                paint_circle(
                    center,
                    CIRCLE_RADIUS - px(1.),
                    CIRCLE_STROKE_WIDTH,
                    colors.lane(circle_color),
                    background,
                    window,
                );
            } else {
                paint_circle(
                    center,
                    CIRCLE_RADIUS + px(1.),
                    CIRCLE_STROKE_WIDTH,
                    colors.lane(circle_color),
                    background,
                    window,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(value: u64) -> Oid {
        let mut bytes = [0u8; 20];
        bytes[12..].copy_from_slice(&value.to_be_bytes());
        Oid::from_bytes(&bytes).unwrap()
    }

    fn commit(value: u64, parents: &[u64], ref_names: &[&str]) -> Arc<InitialGraphCommitData> {
        Arc::new(InitialGraphCommitData {
            sha: oid(value),
            parents: parents.iter().copied().map(oid).collect(),
            ref_names: ref_names
                .iter()
                .map(|name| SharedString::new(*name))
                .collect(),
        })
    }

    #[test]
    fn linear_history_keeps_one_lane() {
        let commits = vec![
            commit(3, &[2], &[]),
            commit(2, &[1], &[]),
            commit(1, &[], &[]),
        ];

        let rows = layout_graph(&commits, &GraphRefs::default());

        assert_eq!(rows.len(), 3);
        assert!(rows[0].input_swimlanes.is_empty());
        assert_eq!(
            rows[0].output_swimlanes,
            vec![Swimlane {
                id: GraphNodeId::Commit(oid(2)),
                color: LaneColor::Palette(0),
            }]
        );
        assert_eq!(rows[1].input_swimlanes, rows[0].output_swimlanes);
        assert_eq!(
            rows[1].output_swimlanes,
            vec![Swimlane {
                id: GraphNodeId::Commit(oid(1)),
                color: LaneColor::Palette(0),
            }]
        );
        assert_eq!(rows[2].input_swimlanes, rows[1].output_swimlanes);
        assert!(rows[2].output_swimlanes.is_empty());
        assert_eq!(graph_width(&rows[0]), LANE_WIDTH * 2);

        let commits = vec![
            commit(3, &[2], &["main"]),
            commit(2, &[1], &[]),
            commit(1, &[], &[]),
        ];
        let rows = layout_graph(
            &commits,
            &GraphRefs {
                badges: BadgeFilter::All,
                ..Default::default()
            },
        );
        assert_eq!(rows[0].badges.len(), 1);
        assert_eq!(rows[0].badges[0].color, LaneColor::Palette(0));
    }

    #[test]
    fn merge_history_creates_and_collapses_side_lane() {
        let commits = vec![
            commit(5, &[4, 2], &["HEAD -> main"]),
            commit(4, &[3], &[]),
            commit(2, &[1], &[]),
            commit(3, &[1], &[]),
            commit(1, &[0], &[]),
            commit(0, &[], &[]),
        ];
        let refs = GraphRefs {
            head_sha: Some(oid(5)),
            current: Some(SharedString::new("main")),
            ..Default::default()
        };

        let rows = layout_graph(&commits, &refs);

        assert_eq!(rows[0].kind, GraphRowKind::Head);
        assert_eq!(rows[0].parents.len(), 2);
        assert_eq!(rows[0].output_swimlanes.len(), 2);
        assert_eq!(
            rows[0].output_swimlanes[0],
            Swimlane {
                id: GraphNodeId::Commit(oid(4)),
                color: LaneColor::Local,
            }
        );
        assert_eq!(
            rows[0].output_swimlanes[1],
            Swimlane {
                id: GraphNodeId::Commit(oid(2)),
                color: LaneColor::Palette(0),
            }
        );

        let collapse_row = rows
            .iter()
            .find(|row| row.input_swimlanes.len() == 2 && row.output_swimlanes.len() == 1)
            .expect("side lane should collapse once it joins the mainline");
        assert_eq!(collapse_row.id, GraphNodeId::Commit(oid(1)));
    }

    #[test]
    fn head_kind_and_badge_sorting() {
        let commits = vec![commit(1, &[], &["HEAD -> main", "origin/main", "tag: v1"])];
        let mut refs = GraphRefs {
            head_sha: Some(oid(1)),
            current: Some(SharedString::new("main")),
            remote: Some(SharedString::new("origin/main")),
            remote_names: vec![SharedString::new("origin")],
            ..Default::default()
        };

        let rows = layout_graph(&commits, &refs);

        assert_eq!(rows[0].kind, GraphRowKind::Head);
        assert_eq!(rows[0].badges.len(), 2);
        assert_eq!(rows[0].badges[0].name.as_ref(), "main");
        assert_eq!(rows[0].badges[0].kind, RefKind::LocalBranch);
        assert_eq!(rows[0].badges[0].color, LaneColor::Local);
        assert_eq!(rows[0].badges[1].name.as_ref(), "origin/main");
        assert_eq!(rows[0].badges[1].kind, RefKind::RemoteBranch);
        assert_eq!(rows[0].badges[1].color, LaneColor::Remote);

        refs.badges = BadgeFilter::All;
        let rows = layout_graph(&commits, &refs);

        assert_eq!(rows[0].badges.len(), 3);
        assert_eq!(rows[0].badges[2].name.as_ref(), "v1");
        assert_eq!(rows[0].badges[2].kind, RefKind::Tag);
    }

    #[test]
    fn merge_base_finds_best_common_ancestor() {
        let commits = vec![
            commit(6, &[5, 3], &[]),
            commit(5, &[4], &[]),
            commit(4, &[2], &[]),
            commit(3, &[2], &[]),
            commit(2, &[1], &[]),
            commit(1, &[], &[]),
        ];

        assert_eq!(merge_base(&commits, oid(6), oid(3)), Some(oid(3)));
        assert_eq!(merge_base(&commits, oid(6), oid(4)), Some(oid(4)));
        assert_eq!(merge_base(&commits, oid(6), oid(99)), None);
    }

    #[test]
    fn outgoing_changes_row_precedes_head() {
        let commits = vec![
            commit(3, &[2], &["HEAD -> main"]),
            commit(2, &[1], &["origin/main"]),
            commit(1, &[], &[]),
        ];
        let refs = GraphRefs {
            head_sha: Some(oid(3)),
            current: Some(SharedString::new("main")),
            remote: Some(SharedString::new("origin/main")),
            show_outgoing_changes: true,
            ..Default::default()
        };

        let rows = layout_graph(&commits, &refs);

        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].kind, GraphRowKind::OutgoingChanges);
        assert_eq!(rows[0].id, GraphNodeId::OutgoingChanges);
        assert_eq!(rows[0].parents.len(), 1);
        assert_eq!(rows[0].parents[0], GraphNodeId::Commit(oid(3)));
        assert_eq!(
            rows[0].output_swimlanes,
            vec![Swimlane {
                id: GraphNodeId::Commit(oid(3)),
                color: LaneColor::Local,
            }]
        );
        assert_eq!(rows[1].kind, GraphRowKind::Head);
        assert_eq!(rows[1].input_swimlanes, rows[0].output_swimlanes);
    }

    #[test]
    fn incoming_changes_row_inserted_at_merge_base() {
        let commits = vec![
            commit(4, &[2], &["origin/main"]),
            commit(3, &[2], &["HEAD -> main"]),
            commit(2, &[1], &[]),
            commit(1, &[], &[]),
        ];
        let refs = GraphRefs {
            head_sha: Some(oid(3)),
            current: Some(SharedString::new("main")),
            remote: Some(SharedString::new("origin/main")),
            show_incoming_changes: true,
            ..Default::default()
        };

        let rows = layout_graph(&commits, &refs);

        assert_eq!(rows.len(), 5);
        assert_eq!(rows[2].kind, GraphRowKind::IncomingChanges);
        assert_eq!(rows[2].id, GraphNodeId::IncomingChanges);
        assert_eq!(rows[2].parents.len(), 1);
        assert_eq!(rows[2].parents[0], GraphNodeId::Commit(oid(2)));
        assert!(
            rows[2]
                .input_swimlanes
                .iter()
                .any(|lane| lane.id == GraphNodeId::IncomingChanges)
        );
        assert_eq!(rows[3].id, GraphNodeId::Commit(oid(2)));
    }

    #[test]
    fn normalizes_remote_head_and_remote_branches() {
        let commits = vec![commit(1, &[], &["origin/HEAD", "origin/x", "y"])];

        let rows = layout_graph(
            &commits,
            &GraphRefs {
                remote_names: vec![SharedString::new("origin")],
                badges: BadgeFilter::All,
                ..Default::default()
            },
        );

        assert_eq!(rows[0].badges.len(), 2);
        assert_eq!(rows[0].badges[0].name.as_ref(), "origin/x");
        assert_eq!(rows[0].badges[0].kind, RefKind::RemoteBranch);
        assert_eq!(rows[0].badges[1].name.as_ref(), "y");
        assert_eq!(rows[0].badges[1].kind, RefKind::LocalBranch);

        let rows = layout_graph(
            &commits,
            &GraphRefs {
                badges: BadgeFilter::All,
                ..Default::default()
            },
        );

        assert_eq!(rows[0].badges.len(), 3);
        assert_eq!(rows[0].badges[0].name.as_ref(), "origin/HEAD");
        assert_eq!(rows[0].badges[0].kind, RefKind::LocalBranch);
    }
}
