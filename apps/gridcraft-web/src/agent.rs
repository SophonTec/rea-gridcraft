//! Same-origin, bounded command bridge for the editor's Agent sidebar.
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use gridcraft_engine::DocState;
use gridcraft_ui_egui::SheetApp;
use serde_json::{Value, json};
use wasm_bindgen::{JsCast as _, JsValue, closure::Closure};

const COMMANDS: &[&str] = &[
    "cell.set",
    "range.setValues",
    "selection.set",
    "home.bold",
    "home.italic",
    "home.fontSize",
    "home.fontColor",
    "home.fillColor",
    "home.numberFormat",
    "home.wrapText",
    "home.horizontalAlign",
    "home.borders",
    "insert.chart",
    "insert.table",
    "data.sort",
    "sheet.rename",
    "sheet.add",
    "sheet.new",
    "column.width",
    "row.height",
    "sheet.autoFitColumns",
    "sheet.autoFitRows",
];

#[derive(Default)]
pub struct Bridge {
    queue: Rc<RefCell<VecDeque<Value>>>,
    session: String,
    snapshot: Option<DocState>,
    revision: String,
    pub busy: bool,
    replies: VecDeque<(String, Value)>,
}

impl Bridge {
    pub fn install(ctx: egui::Context) -> Self {
        let bridge = Self::default();
        let Some(window) = web_sys::window() else { return bridge };
        let Ok(origin) = window.location().origin() else { return bridge };
        let own_window = window.clone();
        let queue = bridge.queue.clone();
        let handler = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
            if event.origin() != origin || !event.source().is_some_and(|source| js_sys::Object::is(&source, &own_window)) {
                return;
            }
            let data = event.data();
            let kind = js_sys::Reflect::get(&data, &JsValue::from_str("type")).ok().and_then(|v| v.as_string());
            if kind.as_deref() != Some("amaranton:agent:request") {
                return;
            }
            let payload = js_sys::Reflect::get(&data, &JsValue::from_str("payload")).ok().and_then(|v| v.as_string());
            if let Some(text) = payload.filter(|s| s.len() <= 131_072)
                && let Ok(value) = serde_json::from_str::<Value>(&text)
                && queue.borrow().len() < 16
            {
                queue.borrow_mut().push_back(value);
                ctx.request_repaint();
            }
        });
        if window.add_event_listener_with_callback("message", handler.as_ref().unchecked_ref()).is_ok() {
            handler.forget();
        }
        bridge
    }

    pub fn tick(&mut self, app: &mut SheetApp, ctx: &egui::Context) {
        let requests: Vec<Value> = self.queue.borrow_mut().drain(..).collect();
        for request in requests {
            let Some(id) = request.get("id").and_then(Value::as_str).filter(|s| !s.is_empty() && s.len() <= 160) else { continue };
            let reply = if let Some((_, reply)) = self.replies.iter().find(|(old, _)| old == id) {
                reply.clone()
            } else {
                let result = self.run(app, &request);
                let reply = match result {
                    Ok(value) => json!({"id": id, "ok": true, "result": value, "revision": revision(app)}),
                    Err(error) => json!({"id": id, "ok": false, "error": error, "revision": revision(app)}),
                };
                self.replies.push_back((id.to_string(), reply.clone()));
                if self.replies.len() > 128 {
                    self.replies.pop_front();
                }
                reply
            };
            if let Some(window) = web_sys::window()
                && let Ok(origin) = window.location().origin()
                && let Ok(text) = serde_json::to_string(&reply)
            {
                let envelope = js_sys::Object::new();
                let _ = js_sys::Reflect::set(&envelope, &"type".into(), &"amaranton:agent:reply".into());
                let _ = js_sys::Reflect::set(&envelope, &"payload".into(), &text.into());
                let _ = window.post_message(&envelope, &origin);
            }
            ctx.request_repaint();
        }
    }

    fn run(&mut self, app: &mut SheetApp, request: &Value) -> Result<Value, String> {
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let session = request.get("session").and_then(Value::as_str).unwrap_or("");
        match method {
            "ping" => return Ok(json!({"app": "gridcraft", "version": 1})),
            "begin" => {
                if self.busy {
                    return Err("已有编辑任务正在执行".into());
                }
                if app.editor.is_some() {
                    return Err("请先完成正在输入的单元格，再发送要求".into());
                }
                if session.is_empty() || session.len() > 80 {
                    return Err("无效的编辑会话".into());
                }
                self.snapshot = Some(app.session.doc().map_err(|e| e.to_string())?.clone());
                self.session = session.to_string();
                self.revision = revision(app);
                self.busy = true;
                return app.session.execute("document.inspect", json!({})).map_err(|e| e.to_string());
            }
            "save" if !self.busy => return app.run("file.save", json!({})),
            _ => {}
        }
        if session != self.session || session.is_empty() {
            return Err("编辑会话已变化".into());
        }
        if method == "end" {
            self.busy = false;
            return Ok(json!({"stopped": true}));
        }
        if revision(app) != self.revision {
            return Err("文档已被修改或切换，请重新发起任务".into());
        }
        if method == "undo" && !self.busy {
            let mut before = self.snapshot.take().ok_or("没有可撤销的 Agent 修改")?;
            before.revision = before.revision.max(app.session.doc().map_err(|e| e.to_string())?.revision).saturating_add(1);
            *app.session.doc_mut().map_err(|e| e.to_string())? = before;
            self.revision = revision(app);
            return Ok(json!({"restored": true}));
        }
        if !self.busy {
            return Err("任务已经停止，未执行命令".into());
        }
        let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
        let result = match method {
            "inspect" => app.session.execute("document.inspect", json!({})).map_err(|e| e.to_string()),
            "read" => app.session.execute("sheet.read", params).map_err(|e| e.to_string()),
            "commands" => {
                let search = params.get("search").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
                let commands: Vec<_> = app
                    .session
                    .commands()
                    .into_iter()
                    .filter(|c| {
                        COMMANDS.contains(&c.id) && (c.id.to_ascii_lowercase().contains(&search) || c.label.to_ascii_lowercase().contains(&search))
                    })
                    .map(|c| serde_json::to_value(c).unwrap_or(Value::Null))
                    .collect();
                Ok(json!(commands))
            }
            "execute" => {
                let command = params.get("command").and_then(Value::as_str).ok_or("缺少 command")?;
                if !COMMANDS.contains(&command) {
                    return Err("当前编辑器未开放此命令".into());
                }
                let result = app.run(command, params.get("params").cloned().unwrap_or_else(|| json!({})));
                app.grid.ensure_visible = true;
                result
            }
            _ => Err("未知编辑器操作".into()),
        };
        self.revision = revision(app);
        result
    }
}

fn revision(app: &SheetApp) -> String {
    app.session.active().map(|d| format!("{}:{}", d.uid, d.revision)).unwrap_or_default()
}
