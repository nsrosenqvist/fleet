//! Plans-view behaviour — Tab/j/k/u/Shift+P/Shift+C dispatch across
//! the sidebar + items focus, pause/resume + complete + unblock
//! actions, on-disk plan reload + ticket-title bulk fetch, lazy
//! per-item issue-detail cache.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::plans::store::PlanStore;
use crate::plans::{Plan, PlanState};
use crate::session::now_ms;

use super::{Action, AppState, PlansFocus, View};

impl AppState {
    pub(in crate::tui) fn handle_key_plans(&mut self, key: KeyEvent) -> Action {
        if key.modifiers.contains(KeyModifiers::SHIFT) && matches!(key.code, KeyCode::Char('P')) {
            self.toggle_selected_plan_pause();
            return Action::None;
        }
        if key.modifiers.contains(KeyModifiers::SHIFT) && matches!(key.code, KeyCode::Char('C')) {
            self.complete_selected_plan();
            return Action::None;
        }
        match key.code {
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Esc | KeyCode::Char('p') => {
                self.view = View::Sessions;
                Action::None
            }
            KeyCode::Tab => {
                self.toggle_plans_focus();
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                match self.plans_focus {
                    PlansFocus::Sidebar => self.move_plans_selection(1),
                    PlansFocus::Items => self.move_plans_item_selection(1),
                }
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                match self.plans_focus {
                    PlansFocus::Sidebar => self.move_plans_selection(-1),
                    PlansFocus::Items => self.move_plans_item_selection(-1),
                }
                Action::None
            }
            KeyCode::Char('u') => {
                self.unblock_selected_plan_item();
                Action::None
            }
            KeyCode::Char('r') => {
                self.focused_issue_cache.clear();
                self.refresh_plans();
                self.refresh_ticket_titles();
                self.refresh_focused_issue();
                Action::None
            }
            _ => Action::None,
        }
    }

    pub(in crate::tui) fn toggle_plans_focus(&mut self) {
        self.plans_focus = match self.plans_focus {
            PlansFocus::Sidebar => PlansFocus::Items,
            PlansFocus::Items => PlansFocus::Sidebar,
        };
        if self.plans_focus == PlansFocus::Items {
            let n_items = self.selected_plan().map_or(0, |p| p.items.len());
            if n_items > 0 && self.plans_items_state.selected().is_none() {
                self.plans_items_state.select(Some(0));
            }
            self.refresh_focused_issue();
        }
    }

    pub(in crate::tui) fn move_plans_item_selection(&mut self, delta: isize) {
        let Some(plan) = self.selected_plan() else {
            return;
        };
        if plan.items.is_empty() {
            return;
        }
        let len = isize::try_from(plan.items.len()).unwrap_or(isize::MAX);
        let current = isize::try_from(self.plans_items_state.selected().unwrap_or(0)).unwrap_or(0);
        let next = (current + delta).rem_euclid(len);
        let next_usize = usize::try_from(next).unwrap_or(0);
        self.plans_items_state.select(Some(next_usize));
        self.refresh_focused_issue();
    }

    fn toggle_selected_plan_pause(&mut self) {
        let Some(idx) = self.plans_list_state.selected() else {
            self.status_line = " no plan selected ".to_string();
            return;
        };
        let Some(plan) = self.plans.get_mut(idx) else {
            return;
        };
        let target = match plan.state {
            PlanState::Active => PlanState::Paused,
            PlanState::Paused => PlanState::Active,
            PlanState::Completed | PlanState::Abandoned => {
                self.status_line = format!(
                    " plan `{}` is {} — pause/resume only applies to active plans ",
                    plan.id,
                    crate::tui::ui::plan_state_word(plan.state),
                );
                return;
            }
        };
        let previous = plan.state;
        plan.state = target;
        plan.updated_at_ms = now_ms();
        let store = PlanStore::for_repo(&self.root);
        match store.save(plan) {
            Ok(()) => {
                self.status_line = format!(
                    " plan `{}` → {} ",
                    plan.id,
                    crate::tui::ui::plan_state_word(target)
                );
            }
            Err(err) => {
                plan.state = previous;
                self.status_line = format!(" plan save failed: {err:#} ");
            }
        }
    }

    fn complete_selected_plan(&mut self) {
        let Some(idx) = self.plans_list_state.selected() else {
            self.status_line = " no plan selected ".to_string();
            return;
        };
        let Some(plan) = self.plans.get_mut(idx) else {
            return;
        };
        if plan.state == PlanState::Completed {
            self.status_line = format!(" plan `{}` already completed ", plan.id);
            return;
        }
        let previous = plan.state;
        plan.state = PlanState::Completed;
        plan.updated_at_ms = now_ms();
        let store = PlanStore::for_repo(&self.root);
        match store.save(plan) {
            Ok(()) => {
                self.status_line = format!(" plan `{}` → completed ", plan.id);
            }
            Err(err) => {
                plan.state = previous;
                self.status_line = format!(" plan save failed: {err:#} ");
            }
        }
    }

    fn unblock_selected_plan_item(&mut self) {
        if self.plans_focus != PlansFocus::Items {
            self.status_line = " press Tab to focus items before unblocking ".to_string();
            return;
        }
        let Some(plan) = self.selected_plan() else {
            return;
        };
        let Some(item_idx) = self.plans_items_state.selected() else {
            self.status_line = " no item selected ".to_string();
            return;
        };
        let Some(item) = plan.items.get(item_idx) else {
            return;
        };
        let ticket = item.ticket_id.clone();
        let deps_store = crate::deps::DepsStore::for_repo(&self.root);
        match deps_store.remove_edges_for_blocked(&ticket) {
            Ok(n) => {
                self.status_line =
                    format!(" ticket `{ticket}`: cleared {n} dep edge{} ", plural(n));
                self.refresh_cycle_nodes_only();
            }
            Err(err) => {
                self.status_line = format!(" unblock failed: {err:#} ");
            }
        }
    }

    fn refresh_cycle_nodes_only(&mut self) {
        let deps_store = crate::deps::DepsStore::for_repo(&self.root);
        let doc = deps_store
            .load()
            .unwrap_or_else(|_| crate::deps::DepsDoc::empty());
        self.cycle_nodes = crate::deps::nodes_in_cycle(&doc);
        self.deps_doc = doc;
    }

    pub(in crate::tui) fn open_plans_view(&mut self) {
        self.refresh_plans();
        self.refresh_ticket_titles();
        self.view = View::Plans;
    }

    fn refresh_ticket_titles(&mut self) {
        let Some(tracker) = self.ensure_tracker() else {
            self.tickets_by_id.clear();
            return;
        };
        let root = self.root.clone();
        match tracker.list_issues(&root) {
            Ok(issues) => {
                self.tickets_by_id.clear();
                for issue in issues {
                    self.tickets_by_id.insert(issue.human_id.clone(), issue);
                }
            }
            Err(err) => {
                self.status_line = format!(" tracker list failed: {err:#} ");
            }
        }
    }

    pub(in crate::tui) fn refresh_plans(&mut self) {
        let store = PlanStore::for_repo(&self.root);
        let prev_id = self
            .plans_list_state
            .selected()
            .and_then(|i| self.plans.get(i))
            .map(|p| p.id.clone());
        let ids = match store.list() {
            Ok(ids) => ids,
            Err(err) => {
                self.status_line = format!(" plans: list failed: {err:#} ");
                self.plans.clear();
                self.plans_list_state.select(None);
                return;
            }
        };
        let mut plans = Vec::with_capacity(ids.len());
        for id in ids {
            match store.load(&id) {
                Ok(p) => plans.push(p),
                Err(err) => {
                    self.status_line = format!(" plans: load `{id}` failed: {err:#} ");
                }
            }
        }
        self.plans = plans;
        let new_index = prev_id
            .and_then(|id| self.plans.iter().position(|p| p.id == id))
            .or(if self.plans.is_empty() { None } else { Some(0) });
        self.plans_list_state.select(new_index);
    }

    pub(in crate::tui) fn move_plans_selection(&mut self, delta: isize) {
        if self.plans.is_empty() {
            return;
        }
        let len = isize::try_from(self.plans.len()).unwrap_or(isize::MAX);
        let current = isize::try_from(self.plans_list_state.selected().unwrap_or(0)).unwrap_or(0);
        let next = (current + delta).rem_euclid(len);
        let next_usize = usize::try_from(next).unwrap_or(0);
        self.plans_list_state.select(Some(next_usize));
        self.plans_items_state.select(None);
    }

    pub(in crate::tui) fn selected_plan(&self) -> Option<&Plan> {
        self.plans_list_state
            .selected()
            .and_then(|i| self.plans.get(i))
    }

    pub(in crate::tui) fn refresh_focused_issue(&mut self) {
        let Some(plan_idx) = self.plans_list_state.selected() else {
            return;
        };
        let Some(plan) = self.plans.get(plan_idx) else {
            return;
        };
        let Some(item_idx) = self.plans_items_state.selected() else {
            return;
        };
        let Some(item) = plan.items.get(item_idx) else {
            return;
        };
        let ticket_id = item.ticket_id.clone();
        if self.focused_issue_cache.contains_key(&ticket_id) {
            return;
        }
        let Some(tracker) = self.ensure_tracker() else {
            self.focused_issue_cache.insert(
                ticket_id,
                Err(format!(
                    "no tracker configured (`tracker: {}` in .fleet/config.yaml is not implemented yet)",
                    self.config.tracker.as_str(),
                )),
            );
            return;
        };
        let root = self.root.clone();
        let result = tracker
            .read(&root, &ticket_id)
            .map_err(|err| format!("{err:#}"));
        self.focused_issue_cache.insert(ticket_id, result);
    }
}

/// "" for `n == 1`, "s" otherwise.
#[must_use]
fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}
