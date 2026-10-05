# xiii-script

Compiled UnrealScript support for XIII Classic (M2c): reflected `UField` /
`UStruct` / `UFunction` / `UState` / `UClass` / `UConst` / `UEnum` / `UProperty` payloads,
class default properties, a bytecode token decoder with explicit failures, a disassembler, a
native-function catalog, a cross-reference with the native DLL export names, and a minimal
interpreter with a native registry (runs a real Plage00 trigger chain headless).

Filesystem-free (`&[u8]` in), bounded (`ScriptLimits`, `xiii_package::Limits`), `unsafe`
forbidden, no dependencies besides `xiii-package`. CLI: `xiii-tool script ...`.

## API

- `read_script_object(package, data, export, &ScriptLimits, &Limits) -> ScriptObject`:
  any reflected Core export. The payload must be consumed to its last byte, otherwise
  `ScriptErrorKind::TrailingBytes`.
- `class_defaults(package, data, export, ...) -> PropertyBlock`: the defaults of a
  `Core.Class` export (after the native class data), ending exactly at the payload end.
- `decode_script(data, start, end, script_size, Tables, is_none_name, &ScriptLimits)`:
  token stream decoder (synthetic tests call it directly).
- `ScriptPackage::load` / `ScriptSet`: decode every reflected export of a package; resolve
  imports by path across loaded packages; native-index table; `find_field` walks
  class/state children and `SuperField` across packages.
- `natives::{native_catalog, CallStats, match_dll_symbols, function_params}`;
  `disasm::Disasm` (statement listing, token tree); `pe::export_names` (PE export table only).

Errors (`ScriptError`) carry the absolute file offset, the memory (code) offset inside the
bytecode, the export index and the field. Unknown opcodes fail with
`UnknownToken { opcode }` at their offsets; nothing is skipped.

## Verified layouts (version 100, licensee 58)

All script lives in the 19 GOG `.u` packages (all licensee 58; no script objects exist in
`.unr/.utx/.usx/.uax/.ukx`, so licensee 50/56/57 have no script layout to compare). The
patched Steam copy (33 `.u`, incl. the `*Plus` packages, also all licensee 58) decodes with
the same layout.

Non-class exports start with `UObject` data (state frame if `RF_HasStack`, tagged-property
block, always empty here); `Core.Class` payloads do not (UClass skips it).

| Object | Fields after the parent part | XIII deviation from stock UE2 / UELib |
|---|---|---|
| UField | SuperField (compact ref), Next (compact ref) | SuperField still in UField (v < 756) |
| UStruct | ScriptText ref, Children ref, FriendlyName (compact name), Line i32, TextPos i32, ScriptSize i32 (**memory** bytes), tokens | no CppText (v < 120), no StructFlags, no struct defaults |
| UFunction | iNative u16, OperatorPrecedence u8, **FunctionFlags 24-bit**, RepOffset u16 if Net | stock: u32 flags with a different bit order (below) |
| UState | ProbeMask u64, IgnoreMask u64, LabelTableOffset u16, **StateFlags u16** | stock: u32 StateFlags |
| UClass | (after UState) **ClassFlags u16**, ClassGuid 16 B, Dependencies (count; compact class ref, u32 deep, u32 script CRC), PackageImports (count; compact names), ClassWithin ref, ClassConfigName name, HideCategories (count; names), **defaults = tagged-property block to the payload end** | stock: u32 ClassFlags. XIII stores 18 bytes for flags+GUID; bytes 2..18 are zero in every class, so the u16/16-byte split is inferred (consistent with the other narrowed flag fields) |
| UConst | value FString | — |
| UEnum | names (count; compact names) | — |
| UProperty | **ArrayDim i16**, PropertyFlags u32 (stock values: Parm 0x80, OutParm 0x100, ReturnParm 0x400, Net 0x20), Category name, RepOffset u16 if Net | ArrayDim 16-bit, as UELib's XIII branch reads it |
| Byte/Object/Class/Struct/Array/Delegate property | Enum / PropertyClass / PropertyClass + MetaClass / Struct / Inner / Function (compact refs) | Map/FixedArray/Pointer properties do not occur and are refused |

XIII `FunctionFlags` (verified against the `engine.u` source text, which is not stripped:
1 765 functions matched to their declarations; every bit below matched its keyword in 100%):
`0x1` final, `0x2` iterator, `0x4` latent, `0x8` preoperator, `0x10` net (RepOffset
follows), `0x20` reliable, `0x40` simulated, `0x80` exec, `0x100` native, `0x200` event,
`0x400` operator, `0x800` static, `0x1000` set on almost every function (meaning not
confirmed), `0x10000` singular, `0x20000` XIII `debugonly`, `0x40000` has a body (stock
`Defined`). Stock UE2 `Defined` (0x2) and `Singular` (0x20) moved up; the rest shifted down.
Note: XIII declares many self-acting natives `static` (e.g. `GotoState`, `IsA`, `Disable`,
`FinishAnim`); an interpreter must still pass `self` to natives.

### Bytecode

Opcodes and operand layouts follow UELib's UE2 table for versions 95..177
(`DefaultEngineBranch.BuildTokenMap`, `src/Core/Tokens/*.cs`). On disk, object references and
names are compact indices but count 4 memory bytes each; decoding stops when the memory
offset reaches `ScriptSize` and must hit it exactly. Jump/label/iterator/case targets are
memory offsets. After a call, one optional `DebugInfo` (0x42, version 100) is consumed as the
engine serializer does (never occurs in the corpus). Native calls: `0x70..=0xFF` = index,
`0x60..=0x6F` + byte = `(op - 0x60) << 8 | byte`. Rejected as unknown (never occur, version-
or engine-specific in UELib): `0x03`, `0x15`, `0x35`, `0x3A`, `0x46`, `0x47`, `0x49..=0x5F`.
No XIII-specific opcode was found.

## Evidence (measured 2026-10-05, `xiii-tool script coverage XIII_Game`)

Report: `docs/evidence/script-coverage-gog.json` (metadata only: class/function names, native
indices, counts; no bytecode bodies, strings or values).

- 19 packages, **31 960 reflected exports, 0 failures**: 6 903 functions, 532 states, 1 444
  classes, 92 structs, 22 632 properties, 253 consts, 104 enums. Every payload consumed to its
  last byte.
- 7 471 scripts with code: **445 474 tokens, 0 unknown**, 1 324 262 memory bytes /
  980 779 file bytes, each ending exactly at its declared size.
  Per game package: `xiii` 102 761 tokens, `xidcine` 36 406, `xidmaps` 12 123,
  `xidpawn` 46 247, `xidinterf` 129 002; `core` 595, `engine` 47 781.
- **Class defaults: 1 444 / 1 444** `Core.Class` exports decode to the payload end (9 572
  default properties). `xiii-tool coverage` now uses this path: 141 939 / 141 939 exports
  decode, 0 failures (was 1 441 `Core.Class` failures). Class defaults are the only place
  where two-byte array indices occur (130, max 254).
- Steam patched copy: 33 packages, 39 573 reflected exports (1 623 classes), 584 712 tokens,
  0 failures.
- Opcodes never seen: `0x34`, `0x3B..=0x3F`, `0x42` (accepted, defined by UELib), plus the
  rejected set.

### Native catalog

- 894 native functions: 418 with an index (412 distinct), 476 bound by name; 8 latent,
  15 iterators, 104 operators.
- **Index conflicts** declared in the licensee source itself: 203 (`Object.!=` rotator and
  `Object.ResetConfig`), 472–476 (`ScriptedTexture` vs `MatchMakingManager`/`VideoPlayer`).
  `xiii` calls 203 once and 476 once, `xidcine` 203 once; the argument count disambiguates,
  the runtime must not pick by index alone.
- Every native index called from bytecode is declared by some function.
- DLL cross-reference (`?exec<Name>@[AU]<Class>@@` exports of `system/*.dll`, 1 002 symbols):
  **880 / 894 natives have a symbol**; unmatched: 9 operators and 5 `ScriptedTexture`
  natives. 122 symbols have no script function (expression-token handlers such as
  `Object.Context`, latent `Poll*` functions, operators with differently named thunks).
- Native calls from game bytecode: `xidmaps` 1 422 (120 distinct indices), `xiii` 12 582
  (195), `xidcine` 4 300 (165). Top entries are operators (`!=`, `&&`, float math, `$`),
  then `GotoState`, `Spawn`, `Destroy`, `SetTimer`, `PlaySound`, canvas drawing.

## Chosen behavior path (input to the interpreter step)

`Plage00.unr`: `TouchTrigger2` (`XIII.TouchTrigger`, Event `tueurs_haut_et_loin`) →
`Touch` → `Actor.TriggerEvent` → `foreach DynamicActors(class'Actor', A, Event)` →
`XIIIDispatcher0.Trigger` (`XIDPawn.XIIIDispatcher`, Tag `tueurs_haut_et_loin`, OutEvents
`tueur_haut`/`tueur_loin`, `bTriggerOnceOnly`) → `GotoState('Dispatch')` → state code loop
with latent `Sleep(OutDelays[i])` and `TriggerEvent(OutEvents[i])` (targets `BaseSoldier15/14`)
→ `GotoState('Fin')` → `stop`. Disassembly is kept outside the repository.

Needed: opcodes Let/LetBool, Instance/Local variables, Context, DynArrayElement,
DynArrayLength, ArrayElement (static `OutEvents[8]`), JumpIfNot/Jump, Skip, Return, Nothing,
Stop, LabelTable, Iterator/IteratorNext/IteratorPop, VirtualFunction, NativeCall,
DynamicCast, NameConst/IntZero/True/False/IntConstByte, Self. Natives: 113 `GotoState`,
118 `Disable`, 129/130/132 bool operators, 150 `<` int, 165 `++` int, 254/255 name `==`/`!=`,
256 `Sleep` (latent), 303 `IsA`, 313 `DynamicActors` (iterator). Engine-side services: the
`Touch` event from collision, `Touching` array, `Tag`/`Event` lookup, `Instigator`, class
defaults (`XIIIDispatcher` has none of its own; `OutDelays` defaults to 0). Script virtual
calls: `TriggerEvent`, `Trigger`, `Touch`. The final targets (`BaseSoldier.Trigger`, AI) are
out of scope for the first harness and must be recorded as an unsupported/diagnostic event,
not a silent success.

## Interpreter (`vm`, `registry`, `value`) — implemented 2026-10-05

Filesystem-free and engine-free: the caller loads packages into a `ScriptSet`, the `Vm`
borrows it.

- **Objects**: `Vm::load_level(map)` instantiates every map export whose class derives from
  `Actor` (Plage00: 371). `ClassLayout` = one slot per property of the class chain (static
  arrays expanded), defaults = zero values, then the class-default blocks root to leaf, then the
  map's tagged properties. Values: `Int/Float/Bool/Byte/Name/Str/Object/Vector/Rotator/Struct/
  Array`; anything the loader cannot convert is `Unsupported(desc)` and reading it is an
  error. Class-default objects (`Default__X`) back `default.` and `class'X'.static` access.
- **Execution**: the decoded token trees are interpreted directly; jump/label/case targets use
  a per-script memory-offset → statement map (a target that is not a statement start is an
  error). Places (l-values): locals, object slots, static-array elements, dynamic-array
  elements (grow on write), struct members. `out` parameters are copied back. `Context` on
  `None` records `AccessedNone` and yields the zero value (UE2 behaviour).
- **Dispatch**: virtual calls look in the current state, its super states, then the class
  chain; `global.` skips states; final calls by reference; native calls by index resolve to
  the declaring function first, and duplicate indices (203, 472–476) are separated by
  argument count (`AmbiguousNative` otherwise).
- **States**: `GotoState` (EndState → switch → label lookup in the state's label table, default
  `Begin`, `Auto` = the state flagged auto → BeginState). State code runs in a resumable frame
  per object during `tick`; `GotoState`/`goto` from state code continue with the new code in the
  same tick (UE2 `ProcessState`). Latent `Sleep` suspends; it finishes when the remaining time
  drops below half a tick (UE2 `execPollSleep`). Iterators (`foreach`) snapshot the native's
  results and assign the `out` parameter per iteration.
- **Tick driver**: `Vm::tick(dt)`: timers (`SetTimer` → `Timer` event), then state code of every
  active object in id order. Step budget (`VmLimits::max_steps`, statements + calls) per tick or
  external call; call depth cap.
- **Scope**: only *active* objects execute. A script call into an inactive object is recorded
  as `Deferred` (an error if the call needs a return value). Engine events honour `Disable`.
- **Failures** (`VmError` with a script stack: function, object, code offset): unsupported
  tokens (e.g. `new`, delegates), unimplemented/unregistered natives, budget, depth, bad
  types/indices/values, latent natives outside state code, assertion failures.
- **Registry**: 47 natives keyed by `Class.Function`, each with signature, evidence and status:
  bool/int/float/string/name/object operators, `GotoState`, `IsA`, `Disable`, `Enable`,
  `Log` (partial: trace only), `Sleep` (latent), `SetTimer`, `DynamicActors` (partial: snapshot
  in map export order; skips `bStatic` actors). Semantics are UE2's, with the XIII declarations
  (index, flags, parameters) decoded from `core.u`/`engine.u` and the `?exec` symbol present
  in the DLLs; none was checked against the original executable's behaviour.

### Measured run (`xiii-tool script run --game-dir XIII_Game --trace`)

Plage00, executed scope = TouchTrigger0–5 + XIIIDispatcher0, dt 1/30 s, `Touch` from a
synthetic `XIIIPlayerPawn` before tick 1:

1. `TouchTrigger2.Touch`: `bActif` (set by its `PostBeginPlay`), `IsA('XIIIPlayerPawn')`
   true → `TriggerEvent('tueurs_haut_et_loin')` → `DynamicActors` → `[Cine8, XIIIDispatcher0]`;
   `Cine8` (`XIDCine.Cine2`) **deferred**; `XIIIDispatcher0.Trigger` → `GotoState('Dispatch')`;
   `Disable('Touch')`.
2. Tick 1: state code `Begin`: `OutEvents[0]='tueur_haut'`, `Sleep(OutDelays[0]=0.0)` suspends.
3. Tick 2: resume; `TriggerEvent('tueur_haut')` → `[BaseSoldier15]` **deferred** (AI);
   `OutEvents[1]`: `Sleep(0.0)`.
4. Tick 3: resume; `[BaseSoldier14]` deferred; `OutEvents[2..7]` are `None`; loop ends;
   `bTriggerOnceOnly` → `GotoState('Fin')` (no labels: state code stops).

12 natives used, all registered. Widening the scope to `BaseSoldier`/`Cine2` stops with
`UnimplementedNative Actor.Spawn (#278)` at `engine.Pawn.PostBeginPlay` (stack printed): the
next native work is visible rather than stubbed.

Known simplifications: `PreBeginPlay`/`BeginPlay` are not run (they need a GameInfo and
mutators); actor iteration order is map export order; `bStatic` filtering for
`DynamicActors` follows UE2's level ordering (an assumption); no collision, so `Touch` is
delivered by the harness; no replication/net roles.

## Tests

- `cargo test -p xiii-script`: synthetic token streams for every token family (memory vs file
  sizes with multi-byte compact indices, labels, calls, native index forms, optional debug
  info), unknown-token offsets (top-level and nested), truncation, overrun, bad
  references, depth/token/size/label/string limits; generated version-100 packages with
  function/state/class/property payloads, class defaults, Net replication offsets, trailing
  bytes, native catalog/parameters and field lookup; PE export reader and `?exec` parsing.
- Interpreter (`vm_tests.rs`, generated package): arithmetic loop with `out` parameter and a
  duplicate native index resolved by argument count; state labels, latent `Sleep` resume time
  and `GotoState` from state code; latent native outside state code; unimplemented,
  unregistered native and unsupported token errors with stack; step budget; inactive objects.
- Opt-in: `XIII_GOG_DIR=... XIII_STEAM_DIR=... cargo test -p xiii-tool --test local_script`
  (counts above and the Plage00 chain trace; prints `SKIPPED` without the variables).

## References

UELib (`EliotVU/Unreal-Library@3207a17e9b294be3d1bf26b18e07ccff7e1d4b0c`, MIT):
`src/Core/Classes/{UField,UStruct,UFunction,UState,UClass}.cs`,
`src/Core/Classes/Props/UProperty.cs` (XIII 16-bit ArrayDim),
`src/ByteCodeDecompiler.cs`, `src/Core/Tokens/*.cs`, `src/Branch/DefaultEngineBranch.cs`,
`src/Branch/PackageObjectLegacyVersion.cs`. Local copies in ignored `.research/uelib-src/`.
UELib's stock `UFunction`/`UState`/`UClass` field widths do not match XIII; the deviations
above were measured. Independent implementation; no code copied. The archived XIII script
repository was not used for this step.
