//! item20 GUI save-slot native declarations.
//!
//! Disk persistence belongs to `xiii-app`, not the filesystem-free VM. Until the menu VM has a
//! host save-store adapter, these natives reject requests explicitly rather than claiming a slot
//! read/write succeeded. `GetMaxNumberOfSavingSlots` reflects the ten-slot GUI range used by the
//! decoded front-end; request/poll operations return false and therefore remain visibly blocked.

use crate::registry::{NativeCtx, NativeDef, NativeFn, NativeOutcome, NativeStatus};
use crate::value::Value;
use crate::vm::{Vm, VmResult};

fn unavailable(vm: &mut Vm<'_>, ctx: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    vm.note(crate::vm::TraceKind::Note(format!(
        "item20 {} unavailable: GUI host save-store adapter is not connected",
        ctx.path
    )));
    if ctx.path.ends_with("GetMaxNumberOfSavingSlots") {
        Ok(NativeOutcome::Value(Value::Int(10)))
    } else {
        Ok(NativeOutcome::Value(Value::Bool(false)))
    }
}

fn def(path: &'static str, signature: &'static str, evidence: &'static str) -> NativeDef {
    NativeDef {
        path,
        signature,
        evidence,
        status: NativeStatus::Partial(
            "menu host save-store adapter is not connected; this call reports failure",
        ),
        short_circuit: None,
        f: unavailable as NativeFn,
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
            assert!(matches!(d.status, NativeStatus::Partial(_)));
            assert!(!d.signature.is_empty() && !d.evidence.is_empty());
        }
    }
}
