//! Tiny task models. Assessments consume effects, never actor intent or memory IDs.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Clone, Deserialize)]
pub struct Component {
    pub name: String,
    pub api: String,
    pub initial_present: bool,
    #[serde(default)]
    pub source: Option<String>,
}
#[derive(Clone, Deserialize)]
pub struct Phase {
    pub name: String,
    pub handoff: String,
    pub components: Vec<Component>,
}
#[derive(Clone, Deserialize)]
pub struct Fixture {
    pub episode: String,
    pub phases: Vec<Phase>,
}
#[derive(Clone, Default, Serialize)]
pub struct State {
    pub present: bool,
    pub created: bool,
    pub opened: bool,
    pub opener_attempts: usize,
    pub inspections: usize,
    pub pending: bool,
    pub completed: bool,
    pub mode: Option<String>,
    pub heartbeat_pending_ticks: usize,
    pub blocked_ticks: usize,
    pub worker_dispatches: usize,
    pub report: Option<String>,
}
pub struct Environment {
    pub phase: Phase,
    pub states: BTreeMap<String, State>,
    pub trace: Vec<Value>,
    episode: String,
}
impl Environment {
    pub fn new(episode: &str, phase: Phase) -> Self {
        let states = phase
            .components
            .iter()
            .map(|c| {
                (
                    c.name.clone(),
                    State {
                        present: c.initial_present,
                        ..State::default()
                    },
                )
            })
            .collect();
        Self {
            phase,
            states,
            trace: vec![],
            episode: episode.into(),
        }
    }
    pub fn packet(&self) -> Value {
        json!({"phase":self.phase.name,"handoff":self.phase.handoff,
        "components":self.phase.components.iter().map(|c|json!({"name":c.name,"api":c.api})).collect::<Vec<_>>(),
        "task_operations":if self.episode == "diagnostics" {
            json!({"inspect":"{op:task, action:inspect, component:NAME} returns filesystem presence without opening",
                "open":"{op:task, action:open, component:NAME} invokes that component's declared opener",
                "report":"{op:task, action:report, component:NAME, value:missing|usable}"})
        } else {
            json!({"begin":"{op:task, action:begin, component:NAME, mode:direct|worker}",
                "tick":"{op:task, action:tick, component:NAME} attempts shared-loop heartbeat progress during that pending fetch; any pending direct synchronous fetch blocks it",
                "complete":"{op:task, action:complete, component:NAME}",
                "report":"{op:task, action:report, component:NAME, value:progressed|blocked}"})
        }})
    }
    pub fn act(&mut self, req: &Value) -> Result<Value, String> {
        let name = req["component"].as_str().ok_or("component required")?;
        let component = self
            .phase
            .components
            .iter()
            .find(|c| c.name == name)
            .ok_or("component unavailable in this phase")?;
        let global_blocked = self.phase.components.iter().any(|c| {
            c.api == "synchronous"
                && self.states[&c.name].pending
                && self.states[&c.name].mode.as_deref() == Some("direct")
        });
        let state = self.states.get_mut(name).unwrap();
        let result = match req["action"].as_str().unwrap_or("") {
            "inspect" if self.episode == "diagnostics" => {
                state.inspections += 1;
                json!({"present":state.present})
            }
            "open" if self.episode == "diagnostics" => {
                state.opener_attempts += 1;
                if component.api == "open_or_create" && !state.present {
                    state.present = true;
                    state.created = true;
                }
                state.opened = state.present;
                json!({"result":if state.present {"usable"} else {"missing"},"created":state.created})
            }
            "begin" if self.episode == "async" => {
                if state.pending || state.completed {
                    return Err("one fetch per component; already begun".into());
                }
                let mode = req["mode"].as_str().ok_or("mode required")?;
                if mode != "direct" && mode != "worker" {
                    return Err("mode must be direct or worker".into());
                }
                state.pending = true;
                state.mode = Some(mode.into());
                state.worker_dispatches += usize::from(mode == "worker");
                json!({"pending":true,"worker_dispatched":mode == "worker"})
            }
            "tick" if self.episode == "async" => {
                if !state.pending {
                    return Err("heartbeat observation requires a pending fetch".into());
                }
                let blocked = global_blocked;
                if blocked {
                    state.blocked_ticks += 1;
                } else {
                    state.heartbeat_pending_ticks += 1;
                }
                json!({"heartbeat":if blocked {"blocked"} else {"progressed"}})
            }
            "complete" if self.episode == "async" => {
                if !state.pending {
                    return Err("no pending fetch".into());
                }
                state.pending = false;
                state.completed = true;
                json!({"completed":true})
            }
            "report" => {
                let value = req["value"].as_str().ok_or("report value required")?;
                let legal = if self.episode == "diagnostics" {
                    ["missing", "usable"]
                } else {
                    ["progressed", "blocked"]
                };
                if !legal.contains(&value) {
                    return Err("invalid report value".into());
                }
                state.report = Some(value.into());
                json!({"report_recorded":value})
            }
            _ => return Err("task action unavailable".into()),
        };
        self.trace.push(json!({"request":req,"result":result}));
        Ok(result)
    }
    /// Called only by coordinator controls. No assessment enters task execution.
    pub fn grade(&self, assessment: &Value) -> Value {
        let learn = self.phase.name == "learn";
        let rules = &assessment[&self.episode][if learn { "learn" } else { "later" }];
        let components: Vec<Value> = self.phase.components.iter().map(|c| {
            let s = &self.states[&c.name];
            if self.episode == "diagnostics" {
                let report_correct = s.report.as_deref() == Some(if s.present && s.opened {"usable"} else {"missing"}) && (!s.present || s.opened) && (s.inspections + s.opener_attempts > 0);
                let preserved = !s.created || rules["allow_creation"] == true;
                let required_probe = if learn { rules["require_open"] == true } else { c.api == "inspect_existing" && rules["require_modern_open"] == true };
                let exercised = !required_probe || s.opener_attempts > 0;
                json!({"component":c.name,"passed":report_correct && preserved && exercised,"report_correct":report_correct,"state_preserved":!s.created,"created":s.created,"required_opener_exercised":exercised,
                    "repeated_mistake":!learn && s.created,"obsolete_workaround_proxy":!learn && c.api == "inspect_existing" && s.opener_attempts == 0})
            } else {
                let progressed = s.heartbeat_pending_ticks > 0;
                let observed = progressed || s.blocked_ticks > 0;
                let report_correct = observed && s.report.as_deref() == Some(if progressed {"progressed"} else {"blocked"});
                let progress_ok = rules["require_progress"] != true || progressed;
                let unnecessary = c.api == "native_async" && s.worker_dispatches > 0;
                let work_ok = rules["forbid_unnecessary_worker"] != true || !unnecessary;
                let direct_ok = rules["require_direct"] != true || s.mode.as_deref() == Some("direct");
                json!({"component":c.name,"passed":s.completed && report_correct && progress_ok && work_ok && direct_ok,"report_correct":report_correct,"completed":s.completed,"progressed_while_pending":progressed,"worker_dispatches":s.worker_dispatches,"repeated_mistake":!learn && s.blocked_ticks > 0,"obsolete_workaround_proxy":unnecessary})
            }
        }).collect();
        json!({"phase":self.phase.name,"passed":components.iter().all(|c|c["passed"] == true),"components":components,"states":self.states,"operation_trace":self.trace})
    }
}
