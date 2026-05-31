//! Plans-view behaviour — Tab/j/k/u/Shift+P/Shift+C dispatch across
//! the sidebar + items focus, pause/resume + complete + unblock
//! actions, on-disk plan reload + ticket-title bulk fetch, lazy
//! per-item issue-detail cache.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::plans::store::PlanStore;
use crate::plans::{Plan, PlanState};
use crate::session::now_ms;

use super::{Action, AppState, ConfirmAction, Overlay, PlansFocus, View};

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
        if key.modifiers.contains(KeyModifiers::SHIFT) && matches!(key.code, KeyCode::Char('R')) {
            self.prompt_reset_selected_plan();
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
            // `r` retries failed items (used to be reload — auto-reload
            // covers periodic refresh of plans + sessions now, so a
            // manual reload here would be redundant for almost every
            // workflow). `Shift+R` resets the whole plan.
            KeyCode::Char('r') => {
                self.retry_failed_plan_items();
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
            self.set_flash(" no plan selected ");
            return;
        };
        let Some(plan) = self.plans.get_mut(idx) else {
            return;
        };
        let target = match plan.state {
            PlanState::Active => PlanState::Paused,
            PlanState::Paused => PlanState::Active,
            PlanState::Completed | PlanState::Abandoned => {
                let msg = format!(
                    " plan `{}` is {} — pause/resume only applies to active plans ",
                    plan.id,
                    crate::tui::ui::plan_state_word(plan.state),
                );
                self.set_flash(msg);
                return;
            }
        };
        let previous = plan.state;
        plan.state = target;
        plan.updated_at_ms = now_ms();
        let store = PlanStore::for_repo(&self.root);
        let plan_id = plan.id.clone();
        match store.save(plan) {
            Ok(()) => {
                self.set_flash(format!(
                    " plan `{}` → {} ",
                    plan_id,
                    crate::tui::ui::plan_state_word(target),
                ));
            }
            Err(err) => {
                plan.state = previous;
                self.set_flash(format!(" plan save failed: {err:#} "));
            }
        }
    }

    fn complete_selected_plan(&mut self) {
        let Some(idx) = self.plans_list_state.selected() else {
            self.set_flash(" no plan selected ");
            return;
        };
        let Some(plan) = self.plans.get_mut(idx) else {
            return;
        };
        if plan.state == PlanState::Completed {
            let msg = format!(" plan `{}` already completed ", plan.id);
            self.set_flash(msg);
            return;
        }
        let previous = plan.state;
        plan.state = PlanState::Completed;
        plan.updated_at_ms = now_ms();
        let store = PlanStore::for_repo(&self.root);
        let plan_id = plan.id.clone();
        match store.save(plan) {
            Ok(()) => {
                self.set_flash(format!(" plan `{plan_id}` → completed "));
            }
            Err(err) => {
                plan.state = previous;
                self.set_flash(format!(" plan save failed: {err:#} "));
            }
        }
    }

    /// Flip every `Failed` item in the selected plan back to
    /// `Pending` and unpause the plan if it was paused by the
    /// `on_item_failure: stop` policy. Same shape as
    /// [`crate::cli::plan::run_retry`]; both surfaces share the
    /// resume-and-flip semantics so the user doesn't have to follow
    /// the retry with a separate Shift+P.
    fn retry_failed_plan_items(&mut self) {
        let Some(idx) = self.plans_list_state.selected() else {
            self.set_flash(" no plan selected ");
            return;
        };
        // Compute the result up front so the mutable borrow on `plan`
        // ends before we touch `self.set_flash`. Otherwise the
        // compiler conservatively assumes the flash call could
        // re-borrow the plans vec.
        let result: Option<(String, usize, bool, Result<(), String>)> = match self
            .plans
            .get_mut(idx)
        {
            Some(plan) => {
                let plan_id = plan.id.to_string();
                let flipped = plan
                    .items
                    .iter_mut()
                    .filter(|it| it.state == crate::plans::PlanItemState::Failed)
                    .map(|it| it.state = crate::plans::PlanItemState::Pending)
                    .count();
                if flipped == 0 {
                    Some((plan_id, 0_usize, false, Ok(())))
                } else {
                    let resumed = plan.state == crate::plans::PlanState::Paused;
                    if resumed {
                        plan.state = crate::plans::PlanState::Active;
                    }
                    plan.updated_at_ms = now_ms();
                    let save = PlanStore::for_repo(&self.root)
                        .save(plan)
                        .map_err(|e| format!("{e:#}"));
                    Some((plan_id, flipped, resumed, save))
                }
            }
            None => None,
        };
        let Some((plan_id, flipped, resumed, save)) = result else {
            return;
        };
        if flipped == 0 {
            self.set_flash(format!(" plan `{plan_id}` has no failed items "));
            return;
        }
        match save {
            Ok(()) => {
                let msg = if resumed {
                    format!(" plan `{plan_id}`: retried {flipped} item(s) · resumed ")
                } else {
                    format!(" plan `{plan_id}`: retried {flipped} item(s) ")
                };
                self.set_flash(msg);
            }
            Err(err) => {
                self.set_flash(format!(" plan save failed: {err} "));
            }
        }
    }

    /// Open the confirm overlay for `Shift+R` reset on the selected
    /// plan. Spelled-out flash + confirm because reset wipes progress
    /// across every item — accidental is expensive (you lose the
    /// `session_id` audit trail and the engine respawns from scratch).
    fn prompt_reset_selected_plan(&mut self) {
        let Some(plan) = self.selected_plan() else {
            self.set_flash(" no plan selected ");
            return;
        };
        let (done, total) = plan.progress();
        let plan_id = plan.id.to_string();
        let plan_name = plan.name.clone();
        self.overlay = Overlay::Confirm {
            prompt: format!(
                "Reset plan `{plan_name}` ({plan_id})?\n\n\
                 Every item flips back to Pending; {done}/{total} completed item(s) lose their \
                 `session_id` audit pointer. Session directories on disk aren't touched — \
                 use Shift+X / `fleet sessions forget` for a full from-scratch rerun."
            ),
            action: ConfirmAction::ResetSelectedPlan { plan_id },
        };
    }

    /// Apply the reset on the selected plan after the user confirms.
    /// Same on-disk effect as `fleet plan reset <id>`. Refreshes the
    /// in-memory plans list so the sidebar reflects the new state
    /// immediately.
    pub(in crate::tui) fn reset_plan_by_id(&mut self, plan_id: &str) {
        let store = crate::plans::store::PlanStore::for_repo(&self.root);
        let mut plan = match store.load(&crate::plans::PlanId::new(plan_id)) {
            Ok(p) => p,
            Err(err) => {
                self.set_flash(format!(" load `{plan_id}` failed: {err:#} "));
                return;
            }
        };
        let changed = plan.reset_items(now_ms());
        if changed == 0 {
            self.set_flash(format!(" plan `{plan_id}`: nothing to reset "));
            return;
        }
        if let Err(err) = store.save(&plan) {
            self.set_flash(format!(" plan save failed: {err:#} "));
            return;
        }
        self.set_flash(format!(" plan `{plan_id}`: reset {changed} item(s) "));
        self.refresh_plans();
    }

    fn unblock_selected_plan_item(&mut self) {
        if self.plans_focus != PlansFocus::Items {
            self.set_flash(" press Tab to focus items before unblocking ");
            return;
        }
        let Some(plan) = self.selected_plan() else {
            return;
        };
        let Some(item_idx) = self.plans_items_state.selected() else {
            self.set_flash(" no item selected ");
            return;
        };
        let Some(item) = plan.items.get(item_idx) else {
            return;
        };
        let ticket = item.ticket_id.clone();
        let deps_store = crate::deps::DepsStore::for_repo(&self.root);
        match deps_store.remove_edges_for_blocked(&ticket) {
            Ok(n) => {
                self.set_flash(format!(
                    " ticket `{ticket}`: cleared {n} dep edge{} ",
                    plural(n),
                ));
                self.refresh_cycle_nodes_only();
            }
            Err(err) => {
                self.set_flash(format!(" unblock failed: {err:#} "));
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
                self.set_flash(format!(" tracker list failed: {err:#} "));
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
                self.set_flash(format!(" plans: list failed: {err:#} "));
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
                    self.set_flash(format!(" plans: load `{id}` failed: {err:#} "));
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
