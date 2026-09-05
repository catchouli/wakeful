//! The actor script contract: compiling Rhai files and running their
//! per-tick update.
//!
//! Scripts never touch Bevy directly — the engine hands each script the
//! state it may react to as arguments, and reads back a single value:
//!
//! ```rhai
//! fn on_update(x, z, player_x, player_z, dt) {
//!     // return nothing to stay put, or [new_x, new_z] to move
//! }
//! ```
//!
//! World state goes in as arguments. The sandbox has exactly two host
//! functions: `say(text)` and `say(text, opts)` collect a line (with
//! placement, timing, and wait options) to show as a speech bubble,
//! and `waiting()` reports whether the actor's wait-mode bubble is
//! still open. There is no file or network access; scripts can only
//! compute.
//!
//! A per-actor `Scope` (owned by the caller) persists script state
//! between ticks.

use std::sync::{Arc, Mutex, PoisonError};

use rhai::{Dynamic, Engine, Map, Position, Scope};

/// One `say` call: the line plus how it should be shown.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Said {
    pub text: String,
    /// Free screen placement in virtual pixels, (0, 0) top-left;
    /// `None` anchors the bubble above the actor instead.
    pub at: Option<[f32; 2]>,
    /// Direction the tail points. The default is down for anchored
    /// bubbles and no tail for free-placed ones.
    pub tail: Option<[f32; 2]>,
    /// Never draw the tail.
    pub no_tail: bool,
    /// Stay open until the player presses confirm.
    pub wait: bool,
    /// Auto-dismiss after this many seconds.
    pub ttl: Option<f64>,
}

/// What one script tick produced: where the actor goes and what it
/// said, in call order.
pub struct Tick {
    /// `None` to stay put, or the new ground position.
    pub position: Option<[f32; 2]>,
    /// Lines the script `say`-ed this tick.
    pub said: Vec<Said>,
}

/// A compiled Rhai script, ready to run against a caller-owned [`Scope`].
pub struct CompiledScript {
    engine: Engine,
    ast: rhai::AST,
    /// `say` output appends here; drained once per tick.
    said: Arc<Mutex<Vec<Said>>>,
    /// Mirrored in by the host each tick; read by `waiting()`.
    waiting: Arc<Mutex<bool>>,
}

impl CompiledScript {
    /// Compiles script text.
    pub fn compile(text: &str) -> Result<Self, rhai::ParseError> {
        let said = Arc::new(Mutex::new(Vec::new()));
        let waiting = Arc::new(Mutex::new(false));
        let mut engine = Engine::new();
        let sink = said.clone();
        engine.register_fn("say", move |line: &str| {
            sink.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(Said {
                    text: line.to_owned(),
                    ..Said::default()
                });
        });
        let sink = said.clone();
        engine.register_fn(
            "say",
            move |line: &str, opts: Map| -> Result<(), Box<rhai::EvalAltResult>> {
                let said = parse_said(line, &opts).map_err(runtime_error)?;
                sink.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(said);
                Ok(())
            },
        );
        let flag = waiting.clone();
        engine.register_fn("waiting", move || {
            *flag.lock().unwrap_or_else(PoisonError::into_inner)
        });
        let ast = engine.compile(text)?;
        Ok(Self {
            engine,
            ast,
            said,
            waiting,
        })
    }

    /// Tells the script whether the actor's wait-mode bubble is still
    /// open; read back through `waiting()` during the next update.
    pub fn set_waiting(&self, waiting: bool) {
        *self.waiting.lock().unwrap_or_else(PoisonError::into_inner) = waiting;
    }

    /// Runs one update tick. The position is `None` when the script (or
    /// its missing `on_update`) wants the actor to stay put; `Some([x,
    /// z])` is the new ground position. `said` carries whatever the
    /// script `say`-ed this tick.
    pub fn update(
        &self,
        scope: &mut Scope,
        x: f32,
        z: f32,
        player_x: f32,
        player_z: f32,
        dt: f32,
    ) -> Result<Tick, Box<rhai::EvalAltResult>> {
        // say() output belongs to the tick that calls it; clear any
        // residue from a previous tick that errored mid-drain.
        self.said
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        // rhai computes in f64; pass doubles in and read the pair out.
        let result: Dynamic = self.engine.call_fn(
            scope,
            &self.ast,
            "on_update",
            (
                x as f64,
                z as f64,
                player_x as f64,
                player_z as f64,
                dt as f64,
            ),
        )?;
        let position = convert_position(result)?;
        let said = std::mem::take(
            self.said
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_mut(),
        );
        Ok(Tick { position, said })
    }
}

fn runtime_error(message: String) -> Box<rhai::EvalAltResult> {
    Box::new(rhai::EvalAltResult::ErrorRuntime(
        message.into(),
        Position::NONE,
    ))
}

/// Reads `say`'s options map. Strict: unknown keys and ill-typed values
/// are runtime errors, so script typos surface at the call site.
fn parse_said(line: &str, opts: &Map) -> Result<Said, String> {
    let mut said = Said {
        text: line.to_owned(),
        ..Said::default()
    };
    for (key, value) in opts {
        match key.as_str() {
            "at" => said.at = Some(pair(value, "at")?),
            "tail" => said.tail = Some(pair(value, "tail")?),
            "no_tail" => said.no_tail = bool_of(value, "no_tail")?,
            "wait" => said.wait = bool_of(value, "wait")?,
            "ttl" => said.ttl = Some(seconds(value, "ttl")?),
            other => return Err(format!("unknown option '{other}'")),
        }
    }
    if said.tail.is_some() && said.no_tail {
        return Err("'tail' and 'no_tail' cannot be combined".into());
    }
    Ok(said)
}

fn pair(value: &Dynamic, key: &str) -> Result<[f32; 2], String> {
    let array = value
        .clone()
        .into_array()
        .map_err(|_| format!("'{key}' must be an [x, y] array"))?;
    if array.len() != 2 {
        return Err(format!("'{key}' must be an [x, y] array"));
    }
    Ok([number(&array[0], key)?, number(&array[1], key)?])
}

fn number(value: &Dynamic, key: &str) -> Result<f32, String> {
    value
        .as_float()
        .map(|f| f as f32)
        .or_else(|_| value.as_int().map(|i| i as f32))
        .map_err(|_| format!("'{key}' must contain numbers"))
}

fn bool_of(value: &Dynamic, key: &str) -> Result<bool, String> {
    value
        .as_bool()
        .map_err(|_| format!("'{key}' must be a bool"))
}

fn seconds(value: &Dynamic, key: &str) -> Result<f64, String> {
    value
        .as_float()
        .or_else(|_| value.as_int().map(|i| i as f64))
        .map_err(|_| format!("'{key}' must be a number"))
}

/// Reads the script's return value: a unit stays put, a two-element
/// array is the new position, anything else is a contract violation.
fn convert_position(result: Dynamic) -> Result<Option<[f32; 2]>, Box<rhai::EvalAltResult>> {
    if result.is_unit() {
        return Ok(None);
    }
    if !result.is_array() {
        return Err(Box::new(rhai::EvalAltResult::ErrorMismatchDataType(
            "[] (two-element position)".into(),
            result.type_name().into(),
            Position::NONE,
        )));
    }
    let array = result.into_array()?;
    if array.len() != 2 {
        return Err(Box::new(rhai::EvalAltResult::ErrorMismatchDataType(
            "[] (two-element position)".into(),
            "array of other length".into(),
            Position::NONE,
        )));
    }
    let mut coords = [0.0f32; 2];
    for (coord, value) in coords.iter_mut().zip(array) {
        *coord = value.as_float()? as f32;
    }
    Ok(Some(coords))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returning_a_pair_moves_the_actor() {
        let script = CompiledScript::compile(
            r"
            fn on_update(x, z, player_x, player_z, dt) {
                [x + dt * 2.0, z]
            }
            ",
        )
        .unwrap();
        let mut scope = Scope::new();
        let moved = script.update(&mut scope, 1.0, 2.0, 0.0, 0.0, 0.5).unwrap();
        assert_eq!(moved.position, Some([2.0, 2.0]));
    }

    #[test]
    fn returning_nothing_stays_put() {
        let script =
            CompiledScript::compile("fn on_update(x, z, player_x, player_z, dt) { }").unwrap();
        let mut scope = Scope::new();
        assert_eq!(
            script
                .update(&mut scope, 1.0, 2.0, 0.0, 0.0, 0.5)
                .unwrap()
                .position,
            None
        );
    }

    #[test]
    fn a_script_can_say_and_the_host_reads_it_back() {
        let script = CompiledScript::compile(
            r#"
            fn on_update(x, z, player_x, player_z, dt) {
                say("hello");
                say("there");
            }
            "#,
        )
        .unwrap();
        let mut scope = Scope::new();
        let tick = script.update(&mut scope, 0.0, 0.0, 0.0, 0.0, 0.5).unwrap();
        assert_eq!(tick.said[0].text, "hello");
        assert_eq!(tick.said[1].text, "there");
    }

    #[test]
    fn said_is_per_tick_so_silence_reads_as_empty() {
        let script = CompiledScript::compile(
            r#"
            fn on_update(x, z, player_x, player_z, dt) {
                if spoke < 1 { say("once"); spoke = 1; }
            }
            "#,
        )
        .unwrap();
        let mut scope = Scope::new();
        scope.push("spoke", 0_i64);
        let first = script.update(&mut scope, 0.0, 0.0, 0.0, 0.0, 0.5).unwrap();
        let second = script.update(&mut scope, 0.0, 0.0, 0.0, 0.0, 0.5).unwrap();
        assert_eq!(first.said[0].text, "once");
        assert!(second.said.is_empty());
    }

    #[test]
    fn say_options_round_trip() {
        let script = CompiledScript::compile(
            r#"
            fn on_update(x, z, player_x, player_z, dt) {
                say("over there", #{at: [10.0, 20.0], tail: [-1.0, 0.5], ttl: 2});
            }
            "#,
        )
        .unwrap();
        let said = script
            .update(&mut Scope::new(), 0.0, 0.0, 0.0, 0.0, 0.5)
            .unwrap()
            .said
            .remove(0);
        assert_eq!(said.text, "over there");
        assert_eq!(said.at, Some([10.0, 20.0]));
        assert_eq!(said.tail, Some([-1.0, 0.5]));
        assert_eq!(said.ttl, Some(2.0));
        assert!(!said.wait);
        assert!(!said.no_tail);
    }

    #[test]
    fn a_wait_say_defaults_to_tying_up_the_script() {
        let script = CompiledScript::compile(
            r#"
            fn on_update(x, z, player_x, player_z, dt) {
                say("press Z", #{wait: true, no_tail: true});
            }
            "#,
        )
        .unwrap();
        let said = script
            .update(&mut Scope::new(), 0.0, 0.0, 0.0, 0.0, 0.5)
            .unwrap()
            .said
            .remove(0);
        assert!(said.wait);
        assert!(said.no_tail);
        assert_eq!(said.ttl, None);
        assert_eq!(said.at, None);
    }

    #[test]
    fn an_unknown_say_option_is_a_runtime_error() {
        let script = CompiledScript::compile(
            r#"
            fn on_update(x, z, player_x, player_z, dt) {
                say("hi", #{colour: "red"});
            }
            "#,
        )
        .unwrap();
        assert!(
            script
                .update(&mut Scope::new(), 0.0, 0.0, 0.0, 0.0, 0.5)
                .is_err()
        );
    }

    #[test]
    fn tail_and_no_tail_conflict() {
        let script = CompiledScript::compile(
            r#"
            fn on_update(x, z, player_x, player_z, dt) {
                say("hi", #{tail: [0.0, -1.0], no_tail: true});
            }
            "#,
        )
        .unwrap();
        assert!(
            script
                .update(&mut Scope::new(), 0.0, 0.0, 0.0, 0.0, 0.5)
                .is_err()
        );
    }

    #[test]
    fn waiting_mirrors_what_the_host_sets() {
        let script = CompiledScript::compile(
            r"
            fn on_update(x, z, player_x, player_z, dt) {
                if waiting() { saw_wait = true; }
            }
            ",
        )
        .unwrap();
        let mut scope = Scope::new();
        scope.push("saw_wait", false);
        script.set_waiting(true);
        script.update(&mut scope, 0.0, 0.0, 0.0, 0.0, 0.5).unwrap();
        assert_eq!(scope.get_value::<bool>("saw_wait"), Some(true));
        script.set_waiting(false);
        script.update(&mut scope, 0.0, 0.0, 0.0, 0.0, 0.5).unwrap();
        // Still true: the script only writes it while waiting.
        assert_eq!(scope.get_value::<bool>("saw_wait"), Some(true));
    }

    #[test]
    fn a_missing_update_is_a_contract_violation() {
        let script = CompiledScript::compile("fn helper() { 42 }").unwrap();
        let mut scope = Scope::new();
        assert!(script.update(&mut scope, 0.0, 0.0, 0.0, 0.0, 0.5).is_err());
    }

    #[test]
    fn a_non_array_return_is_rejected() {
        let script =
            CompiledScript::compile("fn on_update(x, z, player_x, player_z, dt) { 7 }").unwrap();
        let mut scope = Scope::new();
        assert!(script.update(&mut scope, 0.0, 0.0, 0.0, 0.0, 0.5).is_err());
    }

    #[test]
    fn a_short_array_return_is_rejected() {
        let script =
            CompiledScript::compile("fn on_update(x, z, player_x, player_z, dt) { [x] }").unwrap();
        let mut scope = Scope::new();
        assert!(script.update(&mut scope, 0.0, 0.0, 0.0, 0.0, 0.5).is_err());
    }

    #[test]
    fn scope_state_persists_between_calls() {
        let script = CompiledScript::compile(
            r"
            fn on_update(x, z, player_x, player_z, dt) {
                if visits < 1 { visits = 0; }
                visits += 1;
                [x + visits, z]
            }
            ",
        )
        .unwrap();
        let mut scope = Scope::new();
        scope.push("visits", 0_i64);
        let first = script.update(&mut scope, 0.0, 0.0, 0.0, 0.0, 0.5).unwrap();
        let second = script.update(&mut scope, 0.0, 0.0, 0.0, 0.0, 0.5).unwrap();
        assert_eq!(first.position, Some([1.0, 0.0]));
        assert_eq!(second.position, Some([2.0, 0.0]));
    }

    #[test]
    fn compile_errors_surface() {
        assert!(CompiledScript::compile("fn broken {").is_err());
    }

    #[test]
    fn scripts_cannot_reach_the_filesystem() {
        // The default engine exposes no file functions; a script trying
        // to call one must fail, not touch the disk.
        let script = CompiledScript::compile(
            r#"
            fn on_update(x, z, player_x, player_z, dt) {
                let f = open_file("/etc/passwd", false);
                [x, z]
            }
            "#,
        )
        .unwrap();
        let mut scope = Scope::new();
        assert!(script.update(&mut scope, 0.0, 0.0, 0.0, 0.0, 0.5).is_err());
    }
}
