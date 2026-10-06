//! item20 GUI save-slot native declarations.
//!
//! GUI save-slot natives delegate to a host-installed provider; the VM itself has no filesystem.

use crate::registry::{NativeCtx, NativeDef, NativeFn, NativeOutcome, NativeStatus};
use crate::value::Value;
use crate::vm::{Vm, VmResult};

/// Metadata for one host save slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveSlotInfo {
    /// User-visible save description.
    pub description: String,
    /// Calendar `[year, month, day, hour, minute]` in UTC.
    pub date: [i32; 5],
}

/// Synchronous save-slot interface installed by a front-end host.
pub trait SaveSlotProvider {
    /// Returns metadata for an occupied slot.
    fn slot(&self, slot: u32) -> Option<SaveSlotInfo>;
    /// Starts an empty-slot query.
    fn request_empty(&mut self, slot: u32) -> bool;
    /// Completes the prior empty-slot query.
    fn poll_empty(&mut self) -> Option<bool>;
    /// Starts a description query.
    fn request_description(&mut self, slot: u32) -> bool;
    /// Completes the prior description query.
    fn poll_description(&mut self) -> Option<String>;
    /// Starts a date/time query.
    fn request_date(&mut self, slot: u32) -> bool;
    /// Completes the prior date/time query.
    fn poll_date(&mut self) -> Option<[i32; 5]>;
    /// Starts reading an occupied slot.
    fn request_read(&mut self, slot: u32) -> bool;
    /// Reports whether the requested read completed.
    fn poll_read(&mut self) -> bool;
    /// Takes the selected slot after the menu calls `LoadAtCheckpoint`.
    fn take_read_slot(&mut self) -> Option<u32>;
}

fn native(vm: &mut Vm<'_>, ctx: &NativeCtx, args: &mut [Value]) -> VmResult<NativeOutcome> {
    let path = ctx.path.rsplit('.').next().unwrap_or(&ctx.path);
    if std::env::var_os("XIII_WATCH_SAVE_SLOTS").is_some() {
        eprintln!("[vm-save-slot] {} args={:?}", ctx.path, args);
    }
    if path == "GetMaxNumberOfSavingSlots" {
        return Ok(NativeOutcome::Value(Value::Int(10)));
    }
    let Some(provider) = vm.save_slots_mut() else {
        vm.note(crate::vm::TraceKind::Note(format!(
            "item20 {path}: no host save-store provider"
        )));
        return Ok(NativeOutcome::Value(Value::Bool(false)));
    };
    let slot_arg = || {
        args.first().and_then(|v| match v {
            Value::Int(n) if *n >= 0 => Some(*n as u32),
            _ => None,
        })
    };
    let ok = match path {
        "RequestIsSlotEmpty" => slot_arg().is_some_and(|s| provider.request_empty(s)),
        "IsSlotEmptyFinished" => {
            if args.len() >= 2 {
                match provider.poll_empty() {
                    Some(empty) => {
                        args[0] = Value::Int(0);
                        args[1] = Value::Bool(empty);
                    }
                    None => {
                        args[0] = Value::Int(-1);
                        args[1] = Value::Bool(true);
                    }
                }
            }
            true
        }
        "RequestGetSlotContentDescription" => {
            slot_arg().is_some_and(|s| provider.request_description(s))
        }
        "IsGetSlotContentDescriptionFinished" => {
            if args.len() >= 2 {
                match provider.poll_description() {
                    Some(description) => {
                        args[0] = Value::Int(0);
                        args[1] = Value::Str(description);
                    }
                    None => {
                        args[0] = Value::Int(-1);
                        args[1] = Value::Str(String::new());
                    }
                }
            }
            true
        }
        "RequestGetSlotContentDateAndTime" => slot_arg().is_some_and(|s| provider.request_date(s)),
        "IsGetSlotContentDateAndTimeFinished" => {
            if args.len() >= 6 {
                match provider.poll_date() {
                    Some(d) => {
                        args[0] = Value::Int(0);
                        for (a, v) in args[1..6].iter_mut().zip(d) {
                            *a = Value::Int(v);
                        }
                    }
                    None => {
                        args[0] = Value::Int(-1);
                        for a in &mut args[1..6] {
                            *a = Value::Int(0);
                        }
                    }
                }
            }
            true
        }
        "RequestReadSlot" => slot_arg().is_some_and(|s| provider.request_read(s)),
        "IsReadSlotFinished" => {
            if let Some(a) = args.first_mut() {
                *a = Value::Int(0);
            }
            provider.poll_read()
        }
        "LoadAtCheckpoint" => {
            if let Some(slot) = provider.take_read_slot() {
                vm.note(crate::vm::TraceKind::Note(format!(
                    "GUI_SAVE_LOAD_SLOT:{slot}"
                )));
                true
            } else {
                false
            }
        }
        _ => false,
    };
    Ok(NativeOutcome::Value(Value::Bool(ok)))
}

fn def(path: &'static str, signature: &'static str, evidence: &'static str) -> NativeDef {
    NativeDef {
        path,
        signature,
        evidence,
        status: NativeStatus::Implemented,
        short_circuit: None,
        f: native as NativeFn,
    }
}

pub fn save_defs() -> Vec<NativeDef> {
    vec![
        def(
            "GUIController.GetMaxNumberOfSavingSlots",
            "native(0) final native static function int GetMaxNumberOfSavingSlots()",
            "gui.u GUIController.GetMaxNumberOfSavingSlots, final native; xidinterf XIIIMenuContinue.InternalOnClick calls it before slot enumeration",
        ),
        def(
            "GUIController.RequestIsSlotEmpty",
            "native(0) final native static function bool RequestIsSlotEmpty(int SlotNumber)",
            "gui.u GUIController.RequestIsSlotEmpty; xidinterf XIIIMenuLoadGameWindow.GetSlotDescription polls IsSlotEmptyFinished",
        ),
        def(
            "GUIController.IsSlotEmptyFinished",
            "native(0) final native static function bool IsSlotEmptyFinished(out int ReturnCode, out bool IsEmpty)",
            "gui.u GUIController.IsSlotEmptyFinished; xidinterf XIIIMenuLoadGameWindow.GetSlotDescription",
        ),
        def(
            "GUIController.RequestGetSlotContentDescription",
            "native(0) final native static function bool RequestGetSlotContentDescription(int SlotNumber)",
            "gui.u GUIController.RequestGetSlotContentDescription; xidinterf XIIIMenuLoadGameWindow.GetSlotDescription",
        ),
        def(
            "GUIController.IsGetSlotContentDescriptionFinished",
            "native(0) final native static function bool IsGetSlotContentDescriptionFinished(out int ReturnCode, out string SlotDesc)",
            "gui.u GUIController.IsGetSlotContentDescriptionFinished; xidinterf XIIIMenuLoadGameWindow.GetSlotDescription",
        ),
        def(
            "GUIController.RequestGetSlotContentDateAndTime",
            "native(0) final native static function bool RequestGetSlotContentDateAndTime(int SlotNumber)",
            "gui.u GUIController.RequestGetSlotContentDateAndTime; xidinterf XIIIMenuLoadGameWindow.GetSlotDescription",
        ),
        def(
            "GUIController.IsGetSlotContentDateAndTimeFinished",
            "native(0) final native static function bool IsGetSlotContentDateAndTimeFinished(out int ReturnCode, out int Year, out int Month, out int Day, out int Hour, out int Min)",
            "gui.u GUIController.IsGetSlotContentDateAndTimeFinished; xidinterf XIIIMenuContinue.STA_CheckDocument",
        ),
        def(
            "GUIController.RequestReadSlot",
            "native(0) final native static function bool RequestReadSlot(int SlotNumber)",
            "gui.u GUIController.RequestReadSlot; xidinterf XIIIMenuLoadGameWindow.LoadFromSlot and XIIIMenuContinue.STA_GetSlotDescription",
        ),
        def(
            "GUIController.IsReadSlotFinished",
            "native(0) final native static function bool IsReadSlotFinished(out int ReturnCode)",
            "gui.u GUIController.IsReadSlotFinished; xidinterf XIIIMenuLoadGameWindow.LoadFromSlot",
        ),
        def(
            "GUIController.LoadAtCheckpoint",
            "native(0) final native static function bool LoadAtCheckpoint(bool ForceLoadAtMapStart)",
            "gui.u GUIController.LoadAtCheckpoint; engine PlayerController.QuickLoad travels with ?load=9",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn item20_menu_native_set_is_unique_and_explicitly_partial() {
        let defs = save_defs();
        assert_eq!(defs.len(), 10);
        let mut paths = std::collections::BTreeSet::new();
        for d in defs {
            assert!(
                paths.insert(d.path.to_ascii_lowercase()),
                "duplicate {}",
                d.path
            );
            assert!(matches!(d.status, NativeStatus::Implemented));
            assert!(!d.signature.is_empty() && !d.evidence.is_empty());
        }
    }
}
