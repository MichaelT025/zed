use std::rc::Rc;
use std::sync::Arc;

use fuzzy::StringMatchCandidate;
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render,
    SharedString, Task, WeakEntity, Window,
};
use picker::{Picker, PickerDelegate, PickerEditorPosition};
use ui::{
    HighlightedLabel, Icon, IconName, IconSize, Label, LabelSize, ListItem, ListItemSpacing,
    prelude::*,
};
use util::ResultExt as _;

/// Which refs the History graph shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum HistoryRefFilter {
    /// Current branch, its upstream, and the branch it was forked from.
    #[default]
    Auto,
    All,
    /// A short ref name, e.g. "main" or "origin/main".
    Branch(SharedString),
}

impl HistoryRefFilter {
    pub(crate) fn label(&self) -> SharedString {
        match self {
            Self::Auto => "Auto".into(),
            Self::All => "All".into(),
            Self::Branch(name) => name.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HistoryRefOption {
    pub name: SharedString,
    pub is_remote: bool,
}

pub(crate) struct HistoryRefPicker {
    picker: Entity<Picker<HistoryRefPickerDelegate>>,
}

impl HistoryRefPicker {
    pub(crate) fn new(
        options: Vec<HistoryRefOption>,
        filter: HistoryRefFilter,
        on_change: Rc<dyn Fn(HistoryRefFilter, &mut Window, &mut App)>,
        width: Rems,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = HistoryRefPickerDelegate {
            history_ref_picker: cx.entity().downgrade(),
            all_options: options,
            matches: Vec::new(),
            filter,
            on_change,
            selected_index: 0,
        };

        let picker = cx.new(|cx| {
            Picker::list(delegate, window, cx)
                .initial_width(width)
                .show_scrollbar(true)
        });

        Self { picker }
    }
}

impl EventEmitter<DismissEvent> for HistoryRefPicker {}

impl Focusable for HistoryRefPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for HistoryRefPicker {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("HistoryRefPicker")
            .child(self.picker.clone())
    }
}

#[derive(Clone, Debug)]
enum HistoryRefEntry {
    Auto,
    All,
    Branch {
        option: HistoryRefOption,
        positions: Vec<usize>,
    },
}

fn sort_matches(matches: &mut [HistoryRefEntry]) {
    matches.sort_by_key(|entry| match entry {
        HistoryRefEntry::Auto => (0, false, String::new()),
        HistoryRefEntry::All => (1, false, String::new()),
        HistoryRefEntry::Branch { option, .. } => (2, option.is_remote, option.name.to_lowercase()),
    });
}

pub(crate) struct HistoryRefPickerDelegate {
    history_ref_picker: WeakEntity<HistoryRefPicker>,
    all_options: Vec<HistoryRefOption>,
    matches: Vec<HistoryRefEntry>,
    filter: HistoryRefFilter,
    on_change: Rc<dyn Fn(HistoryRefFilter, &mut Window, &mut App)>,
    selected_index: usize,
}

impl HistoryRefPickerDelegate {
    fn is_active(&self, entry: &HistoryRefEntry) -> bool {
        match entry {
            HistoryRefEntry::Auto => self.filter == HistoryRefFilter::Auto,
            HistoryRefEntry::All => self.filter == HistoryRefFilter::All,
            HistoryRefEntry::Branch { option, .. } => {
                self.filter == HistoryRefFilter::Branch(option.name.clone())
            }
        }
    }

    fn dismiss(&self, cx: &mut Context<Picker<Self>>) {
        self.history_ref_picker
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .ok();
    }
}

impl PickerDelegate for HistoryRefPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "history ref picker"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix.min(self.matches.len().saturating_sub(1));
        cx.notify();
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Select a branch to show…".into()
    }

    fn editor_position(&self) -> PickerEditorPosition {
        PickerEditorPosition::End
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        if query.is_empty() {
            self.matches = std::iter::once(HistoryRefEntry::Auto)
                .chain(std::iter::once(HistoryRefEntry::All))
                .chain(
                    self.all_options
                        .iter()
                        .cloned()
                        .map(|option| HistoryRefEntry::Branch {
                            option,
                            positions: Vec::new(),
                        }),
                )
                .collect();
            sort_matches(&mut self.matches);
            self.selected_index = self
                .selected_index
                .min(self.matches.len().saturating_sub(1));
            cx.notify();
            return Task::ready(());
        }

        let candidates = self
            .all_options
            .iter()
            .enumerate()
            .map(|(index, option)| StringMatchCandidate::new(index, option.name.as_ref()))
            .collect::<Vec<StringMatchCandidate>>();
        let all_options = self.all_options.clone();

        cx.spawn_in(window, async move |picker, cx| {
            let matches = fuzzy::match_strings(
                &candidates,
                &query,
                false,
                true,
                10000,
                &Default::default(),
                cx.background_executor().clone(),
            )
            .await
            .into_iter()
            .filter_map(|candidate| {
                let option = all_options.get(candidate.candidate_id)?.clone();
                Some(HistoryRefEntry::Branch {
                    option,
                    positions: candidate.positions,
                })
            })
            .collect::<Vec<_>>();

            picker
                .update(cx, |picker, _| {
                    picker.delegate.matches = matches;
                    picker.delegate.selected_index = 0;
                })
                .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(entry) = self.matches.get(self.selected_index()).cloned() else {
            return;
        };

        let filter = match entry {
            HistoryRefEntry::Auto => HistoryRefFilter::Auto,
            HistoryRefEntry::All => HistoryRefFilter::All,
            HistoryRefEntry::Branch { option, .. } => HistoryRefFilter::Branch(option.name),
        };
        self.filter = filter.clone();
        (self.on_change)(filter, window, cx);
        self.dismiss(cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.dismiss(cx);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let entry = self.matches.get(ix)?;
        let is_active = self.is_active(entry);

        let (icon, title, detail) = match entry {
            HistoryRefEntry::Auto => (
                IconName::Sparkle,
                Label::new("Auto").into_any_element(),
                Some("Current branch, upstream, and base"),
            ),
            HistoryRefEntry::All => (
                IconName::GitGraph,
                Label::new("All Branches").into_any_element(),
                None,
            ),
            HistoryRefEntry::Branch { option, positions } => (
                if option.is_remote {
                    IconName::Cloud
                } else {
                    IconName::GitBranch
                },
                HighlightedLabel::new(option.name.clone(), positions.clone())
                    .single_line()
                    .truncate()
                    .into_any_element(),
                None,
            ),
        };

        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap_2()
                        .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
                        .child(v_flex().flex_1().min_w_0().child(title).when_some(
                            detail,
                            |this, detail| {
                                this.child(
                                    Label::new(detail)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .truncate(),
                                )
                            },
                        ))
                        .when(is_active, |this| {
                            this.child(
                                Icon::new(IconName::Check)
                                    .size(IconSize::Small)
                                    .color(Color::Accent),
                            )
                        }),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared(name: &str) -> SharedString {
        SharedString::from(name.to_string())
    }

    #[test]
    fn label_describes_filter() {
        assert_eq!(HistoryRefFilter::Auto.label().to_string(), "Auto");
        assert_eq!(HistoryRefFilter::All.label().to_string(), "All");
        assert_eq!(
            HistoryRefFilter::Branch(shared("origin/main"))
                .label()
                .to_string(),
            "origin/main"
        );
    }

    #[test]
    fn matches_put_auto_and_all_first_then_locals_before_remotes() {
        let mut matches = vec![
            HistoryRefEntry::Branch {
                option: HistoryRefOption {
                    name: shared("origin/zeta"),
                    is_remote: true,
                },
                positions: Vec::new(),
            },
            HistoryRefEntry::All,
            HistoryRefEntry::Branch {
                option: HistoryRefOption {
                    name: shared("main"),
                    is_remote: false,
                },
                positions: Vec::new(),
            },
            HistoryRefEntry::Auto,
            HistoryRefEntry::Branch {
                option: HistoryRefOption {
                    name: shared("Beta"),
                    is_remote: false,
                },
                positions: Vec::new(),
            },
            HistoryRefEntry::Branch {
                option: HistoryRefOption {
                    name: shared("origin/alpha"),
                    is_remote: true,
                },
                positions: Vec::new(),
            },
        ];

        sort_matches(&mut matches);

        let names = matches
            .iter()
            .map(|entry| match entry {
                HistoryRefEntry::Auto => "Auto".to_string(),
                HistoryRefEntry::All => "All".to_string(),
                HistoryRefEntry::Branch { option, .. } => option.name.to_string(),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            ["Auto", "All", "Beta", "main", "origin/alpha", "origin/zeta"]
        );
    }
}
