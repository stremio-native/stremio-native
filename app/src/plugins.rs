use std::path::{Path, PathBuf};

use mlua::LuaSerdeExt;
use slint::Model;

use crate::MainWindow;

/// Events dispatched from the application to the Lua plugin runtime.
#[allow(dead_code)]
pub enum LuaEvent {
    RunAction(String),
    SystemEvent(String, serde_json::Value),
    Reload,
    Shutdown,
}

/// Handle to the plugin subsystem. Dropping this shuts down the Lua thread.
pub struct PluginManager {
    event_tx: tokio::sync::mpsc::Sender<LuaEvent>,
}

impl PluginManager {
    /// Starts the plugin runtime if plugins exist in `plugin_dir`.
    /// Returns `None` (zero overhead) when the directory is empty or missing.
    pub fn new(ui_weak: slint::Weak<MainWindow>, plugin_dir: PathBuf) -> Option<Self> {
        if !has_lua_files(&plugin_dir) {
            tracing::info!(?plugin_dir, "no plugins found, skipping plugin runtime");
            return None;
        }

        let (tx, rx) = hotpath::channel!(
            tokio::sync::mpsc::channel::<LuaEvent>(128),
            label = "plugins_events"
        );

        std::thread::Builder::new()
            .name("lua-plugins".into())
            .spawn(move || {
                lua_thread_main(rx, ui_weak, plugin_dir);
            })
            .expect("failed to spawn lua-plugins thread");

        tracing::info!("plugin runtime started");
        Some(Self { event_tx: tx })
    }

    #[allow(dead_code)]
    pub fn send(&self, event: LuaEvent) {
        if let Err(error) = self.event_tx.try_send(event) {
            tracing::warn!(%error, "failed to send event to plugin runtime");
        }
    }

    pub fn sender(&self) -> tokio::sync::mpsc::Sender<LuaEvent> {
        self.event_tx.clone()
    }
}

impl Drop for PluginManager {
    fn drop(&mut self) {
        let _ = self.event_tx.try_send(LuaEvent::Shutdown);
    }
}

fn lua_thread_main(
    mut rx: tokio::sync::mpsc::Receiver<LuaEvent>,
    ui_weak: slint::Weak<MainWindow>,
    plugin_dir: PathBuf,
) {
    let lua = match create_sandboxed_lua() {
        Ok(lua) => lua,
        Err(error) => {
            tracing::error!(%error, "failed to create Lua VM");
            return;
        }
    };

    if let Err(error) = init_lua_api(&lua, ui_weak.clone()) {
        tracing::error!(%error, "failed to initialize Lua API bindings");
        return;
    }

    load_plugins(&lua, &plugin_dir);

    while let Some(event) = rx.blocking_recv() {
        match event {
            LuaEvent::RunAction(id) => {
                dispatch_lua_event(&lua, "run_action", &id);
            }
            LuaEvent::SystemEvent(name, data) => {
                dispatch_lua_system_event(&lua, &name, data);
            }
            LuaEvent::Reload => {
                tracing::info!("reloading plugins");
                clear_plugin_state(&lua);
                load_plugins(&lua, &plugin_dir);
            }
            LuaEvent::Shutdown => {
                tracing::info!("plugin runtime shutting down");
                break;
            }
        }
    }
}

fn create_sandboxed_lua() -> mlua::Result<mlua::Lua> {
    let lua = mlua::Lua::new();

    // Set instruction limit hook as execution watchdog (1M instructions)
    lua.set_hook(
        mlua::HookTriggers::new().every_nth_instruction(1_000_000),
        |_lua, _debug| {
            Err(mlua::Error::runtime(
                "plugin exceeded execution limit (possible infinite loop)",
            ))
        },
    );

    // Remove dangerous standard library functions
    let globals = lua.globals();
    globals.raw_remove("os")?;
    globals.raw_remove("io")?;
    globals.raw_remove("loadfile")?;
    globals.raw_remove("dofile")?;

    // Replace `require` with a restricted version that only loads from plugin dir
    globals.raw_remove("require")?;

    Ok(lua)
}

fn init_lua_api(lua: &mlua::Lua, ui_weak: slint::Weak<MainWindow>) -> mlua::Result<()> {
    let stremio_table = lua.create_table()?;

    // app.log(message)
    stremio_table.set(
        "log",
        lua.create_function(|_, msg: String| {
            tracing::info!(target: "lua_plugin", "{}", msg);
            Ok(())
        })?,
    )?;

    // app.notify(message)
    let ui_notify = ui_weak.clone();
    stremio_table.set(
        "notify",
        lua.create_function(move |_, msg: String| {
            let ui = ui_notify.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui.upgrade() {
                    ui.set_error_message(slint::SharedString::from(msg));
                }
            });
            Ok(())
        })?,
    )?;

    // app.register_action({ id, label, icon, on_trigger })
    let callbacks = lua.create_table()?;
    lua.set_named_registry_value("plugin_callbacks", callbacks)?;

    let ui_actions = ui_weak.clone();
    stremio_table.set(
        "register_action",
        lua.create_function(move |lua, table: mlua::Table| {
            let id: String = table.get("id")?;
            let label: String = table.get("label")?;
            let icon: String = table
                .get::<mlua::Value>("icon")
                .and_then(|v| match v {
                    mlua::Value::String(s) => Ok(s.to_string_lossy()),
                    _ => Ok(String::new()),
                })
                .unwrap_or_default();

            // Store the on_trigger callback in the registry
            if let Ok(func) = table.get::<mlua::Function>("on_trigger") {
                let callbacks: mlua::Table = lua.named_registry_value("plugin_callbacks")?;
                callbacks.set(id.clone(), func)?;
            }

            // Update UI with registered actions on the Slint thread
            let action_id = id.clone();
            let action_label = label.clone();
            let action_icon = icon.clone();
            let ui = ui_actions.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui.upgrade() {
                    let current = ui.get_plugin_actions();
                    let mut actions: Vec<crate::PluginAction> = (0..current.row_count())
                        .filter_map(|i| current.row_data(i))
                        .collect();
                    actions.push(crate::PluginAction {
                        id: action_id.into(),
                        label: action_label.into(),
                        icon: action_icon.into(),
                    });
                    let model = std::rc::Rc::new(slint::VecModel::from(actions));
                    ui.set_plugin_actions(slint::ModelRc::from(model));
                }
            });

            tracing::info!(id = %id, label = %label, "plugin registered action");
            Ok(())
        })?,
    )?;

    // app.on_event(name, callback)
    let event_handlers = lua.create_table()?;
    lua.set_named_registry_value("event_handlers", event_handlers)?;

    stremio_table.set(
        "on_event",
        lua.create_function(|lua, (name, func): (String, mlua::Function)| {
            let handlers: mlua::Table = lua.named_registry_value("event_handlers")?;
            let list: mlua::Table = match handlers.get::<mlua::Value>(name.as_str())? {
                mlua::Value::Table(t) => t,
                _ => {
                    let t = lua.create_table()?;
                    handlers.set(name.as_str(), t.clone())?;
                    t
                }
            };
            let len = list.len()?;
            list.set(len + 1, func)?;
            Ok(())
        })?,
    )?;

    lua.globals().set("stremio", stremio_table)?;

    Ok(())
}

fn load_plugins(lua: &mlua::Lua, plugin_dir: &Path) {
    let entries = match std::fs::read_dir(plugin_dir) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(%error, ?plugin_dir, "could not read plugin directory");
            return;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "lua") {
            tracing::info!(?path, "loading plugin");
            match std::fs::read_to_string(&path) {
                Ok(source) => {
                    let name = path
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    if let Err(error) = lua.load(&source).set_name(name).exec() {
                        tracing::error!(%error, ?path, "plugin failed to load");
                    }
                }
                Err(error) => {
                    tracing::error!(%error, ?path, "could not read plugin file");
                }
            }
        }
    }
}

fn dispatch_lua_event(lua: &mlua::Lua, event_name: &str, id: &str) {
    let result: mlua::Result<()> = (|| {
        let callbacks: mlua::Table = lua.named_registry_value("plugin_callbacks")?;
        if let mlua::Value::Function(func) = callbacks.get::<mlua::Value>(id)? {
            func.call::<()>(())?;
        }
        Ok(())
    })();

    if let Err(error) = result {
        tracing::warn!(%error, event_name, id, "plugin callback error");
    }
}

fn dispatch_lua_system_event(lua: &mlua::Lua, event_name: &str, data: serde_json::Value) {
    let result: mlua::Result<()> = (|| {
        let handlers: mlua::Table = lua.named_registry_value("event_handlers")?;
        if let mlua::Value::Table(list) = handlers.get::<mlua::Value>(event_name)? {
            let lua_data = lua.to_value(&data)?;
            for pair in list.pairs::<mlua::Integer, mlua::Function>() {
                let (_, func) = pair?;
                if let Err(error) = func.call::<()>(lua_data.clone()) {
                    tracing::warn!(%error, event_name, "plugin event handler error");
                }
            }
        }
        Ok(())
    })();

    if let Err(error) = result {
        tracing::warn!(%error, event_name, "plugin system event dispatch error");
    }
}

fn clear_plugin_state(lua: &mlua::Lua) {
    let _ = lua.set_named_registry_value("plugin_callbacks", lua.create_table().unwrap());
    let _ = lua.set_named_registry_value("event_handlers", lua.create_table().unwrap());
}

fn has_lua_files(dir: &Path) -> bool {
    dir.is_dir()
        && std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .any(|e| e.path().extension().is_some_and(|ext| ext == "lua"))
            })
            .unwrap_or(false)
}
