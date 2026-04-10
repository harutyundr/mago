# Mago Return Type Inference

## Goal

Enhance Mago (the Rust-based PHP static analyzer) to infer return types from method bodies when no explicit type declaration (`function foo(): Type`) or docblock annotation (`@return Type`) exists. This is critical for analyzing XenForo framework code, which extensively uses patterns like:

```php
// XF\Language::phrase() has no return type annotation but returns new Phrase(...)
public function phrase($name, array $params = []) {
    return new Phrase($this, $name, $params);
}

// XF::phrase() calls through to Language::phrase() via static::language()->phrase()
public static function phrase($name, ...) {
    return static::app()->language()->phrase($name, ...);
}
```

Without inference, all such methods are typed as `mixed`, causing cascading `mixed-method-access` and `mixed-argument` errors throughout user code.

---

## Architecture: Mago's 3-Phase Pipeline

### Phase 1 — Scanning (parallel, per-file)
- **File**: `mago/crates/codex/src/scanner/` (entry: `function_like.rs`)
- Each PHP file is scanned independently in parallel
- Scanner `Context` contains ONLY: `arena`, `file`, `program` (AST), `resolved_names`
- **NO access to the codebase** — it's being built from these scan results
- AST (method bodies) is ONLY available here — discarded after scanning
- Output: partial `CodebaseMetadata` per file
- Performs simple type inference (literals, `$this`, `new ClassName()`, variable assignments)
- Collects "hints" for complex cases requiring codebase access

### Phase 2 — Population (sequential, full codebase)
- **File**: `mago/crates/codex/src/populator/mod.rs` (entry: `populate_codebase_inner()`)
- All partial metadata is merged into a single `CodebaseMetadata`
- Full codebase available: can look up any class, method, function
- Resolves type signatures, inheritance hierarchies, symbol references
- **AST is NOT available** — only metadata remains
- Pipeline order: sort classes → populate hierarchy → populate function signatures → populate class types → populate constants → build descendant maps
- Resolves hints collected during scan using fixed-point iteration

### Phase 3 — Analysis (parallel, read-only codebase)
- **File**: `mago/crates/codex/src/analyzer/`
- Each file analyzed with `Arc<CodebaseMetadata>` (read-only)
- Reports errors like `mixed-method-access`, `mixed-argument`, etc.

### The Gap
- AST (needed to see `return new Phrase(...)`) is only in Phase 1
- Cross-method lookups (needed to resolve `$this->getLanguage()->phrase()`) require Phase 2's codebase
- **Solution**: Store simplified "hints" during scanning, resolve them during population

---

## Problem Statement

XenForo framework code often lacks explicit return type hints, causing:

1. **Empty arrays typed incorrectly**: `[]` was inferred as `list{}` (non-empty list) instead of `array<array-key, mixed>`
2. **Method chains return `mixed`**: Calls like `\XF::app()->language()->dateTime()` couldn't be resolved
3. **Variable-based returns not handled**: Common pattern `$x = new Foo(); return $x;` failed to infer type

---

## Implementation

### 1. Empty Array Type Fix

**File**: `mago/crates/codex/src/scanner/inference/mod.rs`

**Change**: Lines 474-496

```rust
Expression::Array(Array { elements, .. }) | Expression::LegacyArray(LegacyArray { elements, .. })
    if is_list_array_expression(expression) =>
{
    if elements.is_empty() {
        return Some(get_mixed_keyed_array());  // Returns array<array-key, mixed>
    }
    
    // ... rest of non-empty array handling
}
```

Empty arrays are typed as generic arrays (`array<array-key, mixed>`), not non-empty lists.

### 2. Property Type Inference from Defaults

**File**: `mago/crates/codex/src/scanner/property.rs`

When a property has no explicit type hint or `@var` docblock, its default value is used:
```php
public $columns = [];  // Inferred as `array` instead of `mixed`
```

Applied in 3 places: plain properties, hooked properties, promoted constructor parameters.

### 3. Simple Type Inference During Scan

**File**: `mago/crates/codex/src/scanner/function_like.rs`

**Function**: `infer_atomic_from_expression()`

Handles immediate cases without needing codebase access:

- `$this` → `static` (TNamedObject with `is_this=true`)
- `true`/`false` → `bool`
- `123` → `int`
- `1.5` → `float`
- `'string'` → `string`
- `null` → `null`
- `new ClassName()` → `ClassName` (resolved via context)
- `$variable` → type of last assignment (see Variable Assignment Tracking)

### 4. Variable Assignment Tracking

**File**: `mago/crates/codex/src/scanner/function_like.rs`

**Function**: `find_last_assignment_in_block()`

```rust
fn find_last_assignment_in_block<'a, 'arena>(
    block: &'a Block<'arena>,
    var_name: &str,
) -> Option<&'a Expression<'arena>> {
    let mut last_rhs: Option<&'a Expression<'arena>> = None;
    for stmt in block.statements.iter() {
        if let Statement::Expression(ExpressionStatement {
            expression: Expression::Assignment(Assignment {
                lhs: Expression::Variable(Variable::Direct(direct)),
                operator: AssignmentOperator::Assign(_),
                rhs,
                ..
            }),
            ..
        }) = stmt {
            if direct.name.eq_ignore_ascii_case(var_name) {
                last_rhs = Some(rhs);
            }
        }
    }
    last_rhs
}
```

Handles patterns like:
```php
public function addColumn($name, $type) {
    $column = new Column($name, $type);
    return $column;  // Correctly inferred as Column
}
```

### 5. Return Expression Hints

**File**: `mago/crates/codex/src/metadata/function_like.rs` (lines 142-160)

```rust
pub enum ReturnExpressionHint {
    InstanceMethodCall { class: Atom, method: Atom },  // $this->method()
    StaticMethodCall { class: Atom, method: Atom },     // ClassName::method()
    FunctionCall { function: Atom },                    // function()
    MethodChain { receiver_class: Atom, methods: Box<[Atom]> },  // $this->a()->b()
}

// On FunctionLikeMetadata:
pub return_expression_hints: Vec<ReturnExpressionHint>,
```

Hints are collected during scan phase when a return expression is too complex to resolve without codebase access.

**Key Functions** in `mago/crates/codex/src/scanner/function_like.rs`:

- `collect_return_expression_hints()` — finds all return statements and extracts hints
- `extract_return_hint()` — converts a return expression to a hint
- `extract_method_chain_from_expression()` — recursively builds method chains

**Important details**:
- Class names are **lowercased** using `ascii_lowercase_atom()` for consistent lookups
- Fully qualified names have leading `\` stripped
- Function names are stored without namespace prefix for fallback resolution
- Variable-based returns (`return $var`) are resolved by finding the last assignment in the block

**Example**:
```php
public function getDateTime() {
    return $this->language()->dateTime();
}
```
Becomes hint: `MethodChain { receiver_class: "xf\app", methods: ["language", "datetime"] }`

### 6. Hint Resolution During Population

**File**: `mago/crates/codex/src/populator/return_hints.rs` (285 lines)

**Integration**: `mago/crates/codex/src/populator/mod.rs` line 169

```rust
// Resolve return types from collected hints
return_hints::resolve_return_expression_hints(codebase);
```

Uses **fixed-point iteration** until no more progress can be made:

```rust
loop {
    let mut changed = false;
    for each function/method with unresolved hints {
        if can resolve hint { set return type; changed = true; }
    }
    if !changed { break; }
}
```

**Key resolution functions**:

1. **`resolve_function_return()`** — tries exact name match, falls back to stripping namespace prefix (`xf\strtr` → `strtr`)
2. **`resolve_method_return()`** — looks up method in class, then parent classes (`all_parent_classes`), then traits (`used_traits`)
3. **`resolve_method_chain()`** — recursively walks the chain: `a()->b()->c()`, each step resolves the receiver for the next
4. **`extract_class_from_type()`** — extracts class name from TUnion; always **lowercases** the result

**Critical**: All class names in mago's metadata are stored lowercase. All lookups must use `ascii_lowercase_atom()`.

---

## PHP Namespace Resolution Rules

PHP has special resolution rules for unqualified function calls:

```php
namespace XF;

function test() {
    strtr(...);  // First tries \XF\strtr, falls back to \strtr
}
```

Implementation:
1. Store function names with full namespace during scan
2. During populate, try exact match first
3. Fall back to global namespace (strip everything before last `\`)

---

## Class Name Normalization

PHP class names are case-insensitive. Mago stores them **lowercase** internally:

```rust
// ❌ WRONG - Will fail lookup
let class_atom = atom("XF\\App");

// ✅ CORRECT - Will succeed
let class_atom = ascii_lowercase_atom("XF\\App");  // becomes "xf\\app"
```

**Everywhere class names are used**: hint extraction during scan, hint resolution during populate, class lookups in metadata maps.

**Exception**: Property names are case-sensitive in PHP — use `atom()` not `ascii_lowercase_atom()`.

---

## Key Gotchas

1. **Variable names include `$`**: PHP variable `$this` is stored as `"$this"` in the AST, not `"this"`. Always use `"$this"` when comparing.

2. **Method names are lowercased**: Keys in `function_likes` use `ascii_lowercase_atom`. Hints must store lowercased method names.

3. **Property names are case-sensitive**: Unlike methods/classes, property names preserve original casing. Use `atom()` not `ascii_lowercase_atom()`.

4. **Global functions key**: For global functions, the key is `(function_fqn, empty_atom())` — the second element is an empty Atom.

5. **`static`/`$this` return type**: `TNamedObject` has `is_this = true`. When resolving chains, if a method returns `static`, the "class" for the next call is the same class.

6. **Fixed-point iteration**: Some methods only become resolvable after other methods in the chain resolve. Multiple passes handle this (capped at ~10 iterations).

7. **`continue` not `?`**: In `infer_return_type_from_block`, when a return statement can't be inferred, use `continue` (skip it) not `?` (which aborts the function returning `None`).

8. **AST keyword representation**: PHP keywords `static`, `self`, `parent` are parsed as dedicated expression types (`Expression::Static`, `Expression::Self_`, `Expression::Parent`), NOT as `Expression::Identifier(Identifier::Local("static"))`.

9. **`TReference::Symbol` vs `TObject::Named`**: `resolve_return_expression_hints` runs before class-like type population. Property types at that point are still `TReference::Symbol`. The `collect_atomics()` function converts them to `TObject::Named` when assembling results.

10. **Avoid recursion in hint extraction**: Pass `None` for block when recursing to prevent infinite loops in `infer_atomic_from_expression`.

---

## Bug Fixes

### Feb 15, 2026 — Property Name `$` Prefix

Property metadata stores names with `$` prefix (e.g., `"$post"`). Hints were storing them without it (e.g., `"post"`), causing all property-based return type inference to fail.

**Fix**: `mago/crates/codex/src/scanner/function_like.rs` line 942-949 — changed to `format!("${}", ident.value)`.

**Impact**: Reduced `mixed-assignment` warnings from 12 to 9 in BHW_OriginalityApi project.

### Feb 15, 2026 — TReference::Symbol in Populate Phase

`resolve_return_expression_hints` runs before class-like type population. Property types were still `TReference::Symbol`, displaying as `unknown-ref(ClassName)` in errors.

**Fix**: `mago/crates/codex/src/populator/return_hints.rs` — `collect_atomics()` function converts `TReference::Symbol` to `TObject::Named`.

**Impact**: PreparerService.php: 23 issues → 0. Full project: 64 issues → 41 (16 fewer errors, zero `unknown-ref` errors).

### Feb 15, 2026 — Static/Self/Parent Keyword AST Handling

`static::`, `self::`, `parent::` were not generating return expression hints because the code only handled `Identifier::Local("static")`, but the parser uses dedicated expression types.

**Fix**: Added `Expression::Static(_)`, `Expression::Self_(_)`, `Expression::Parent(_)` match arms in `extract_return_hint()` and `extract_method_chain_from_expression()`.

### Feb 15, 2026 — Property Name Case Sensitivity

Property names in hints were lowercased via `ascii_lowercase_atom()`, but property metadata stores them with original casing. PHP property names are case-sensitive, so lookups failed.

**Fix**: Replaced `ascii_lowercase_atom(&name_with_dollar)` with `atom(&name_with_dollar)` in the `PropertyAccess` case of `extract_return_hint()`.

**Cumulative impact**: 41 issues (11 errors) → 13 issues (3 errors, 10 warnings) — 28 fewer issues, zero regressions.

---

## Files Modified

| File | Description |
|------|-------------|
| `mago/crates/codex/src/scanner/inference/mod.rs` | Empty array fix |
| `mago/crates/codex/src/metadata/function_like.rs` | `ReturnExpressionHint` enum + field on `FunctionLikeMetadata` |
| `mago/crates/codex/src/scanner/function_like.rs` | Hint collection, variable tracking, property name fix |
| `mago/crates/codex/src/populator/return_hints.rs` | NEW: hint resolution with fixed-point iteration |
| `mago/crates/codex/src/populator/mod.rs` | Integration point |
| `mago/crates/codex/src/scanner/property.rs` | Property inference from defaults |
| `mago/crates/codex/src/scanner/class_like.rs` | Enum method constructors |

---

## Regression Verification

Run this after every feature addition, upstream merge, or refactor. The baseline is **0 issues** (2 known `missing-return-type` entries are suppressed in `.mago-analyzer-baseline.toml`).

### Step 1 — Build

```bash
cargo build
```

Must compile with zero errors. Warnings are acceptable.

### Step 2 — Full Analysis

```bash
./target/debug/mago --workspace /Users/harutyun/Projects/XenForo2/BHW/BHW_OriginalityApi analyse
```

Expected output ends with:
```
Filtered out 2 issues based on the baseline file.
No issues found.
```

Any output other than this is a regression.

### Step 3 — Check Specific Error Categories

These are the error types that were previously caused by broken type inference. If any appear, it means type inference regressed:

```bash
# Should print nothing
./target/debug/mago --workspace /Users/harutyun/Projects/XenForo2/BHW/BHW_OriginalityApi analyse 2>&1 | grep -E "unknown-ref|invalid-property-access|mixed-method-access|mixed-argument"
```

### Baseline Files

Located in the test project (not in this repo):
- `/Users/harutyun/Projects/XenForo2/BHW/BHW_OriginalityApi/.mago-analyzer-baseline.toml` — 2 suppressed `missing-return-type` issues in generated base classes
- `/Users/harutyun/Projects/XenForo2/BHW/BHW_OriginalityApi/.mago-linter-baseline.toml` — empty

Do **not** add new entries to the baseline to hide regressions. Fix the root cause instead.

---

## Testing

Test specific file:
```bash
./target/debug/mago --workspace /Users/harutyun/Projects/XenForo2/BHW/BHW_OriginalityApi analyse upload/src/addons/BHW/OriginalityApi/XF/Service/Post/PreparerService.php
```

Test full project:
```bash
./target/debug/mago --workspace /Users/harutyun/Projects/XenForo2/BHW/BHW_OriginalityApi analyse
```

Verification commands:
```bash
# Check for any remaining unknown-ref errors
./target/debug/mago --workspace /Users/harutyun/Projects/XenForo2/BHW/BHW_OriginalityApi analyse 2>&1 | grep "unknown-ref"

# Check for invalid-property-access on XF types
./target/debug/mago --workspace /Users/harutyun/Projects/XenForo2/BHW/BHW_OriginalityApi analyse 2>&1 | grep "invalid-property-access.*XF"
```

### Specific Test Cases

1. **`\XF::phrase()`** should resolve to `XF\Phrase`:
   - `XF::phrase()` → `static::app()->language()->phrase()` → `new Phrase(...)` → `XF\Phrase`

2. **`$finder->where()->fetchOne()`** should not produce `mixed-method-access`:
   - `Finder::where()` returns `$this` (resolved by simple inference)

3. **`$structure->columns`** should be `array`, not `mixed`:
   - Property has default `= []` (resolved by property inference)

4. **`XF\Language::dateTime()`** should resolve to `string`:
   - `dateTime()` → `$this->getDateTimeOutput()` → `strtr(...)` → `string`

---

## Key Learnings

1. **Separate concerns**: Simple inference in scan phase, complex in populate phase
2. **Case sensitivity**: Lowercase class/method names, preserve property names
3. **Namespace handling**: Functions need fallback to global namespace, classes don't
4. **Inheritance**: Check parent classes AND traits for methods
5. **Fixed-point iteration**: Required for recursive/circular type dependencies
6. **AST patterns**: Use pattern matching to extract semantic information; keywords like `static`/`self`/`parent` are dedicated expression types
7. **Last assignment wins**: Simple linear scan works for most cases
8. **Funnel points**: `collect_atomics()` is the single point where resolved types flow — fixing it handles property access, method calls, and chains
9. **TReference vs TObject**: `TReference::Symbol` is unresolved (`unknown-ref(X)`); must convert to `TObject::Named` when assembling results
10. **Pipeline timing**: Understand when each phase runs; hint resolution precedes type population, so handle unresolved references manually

---

## Future Enhancements

1. **Property type inference**: Track property assignments across methods
2. **Parameter type inference**: Infer from usage patterns
3. **More complex control flow**: Handle if/else branches, loops
4. **Cross-file variable tracking**: Track variables across method boundaries
5. **Array element types**: Infer element types from array operations

---

## References

- Mago AST types: `mago/crates/syntax/src/ast/ast/`
- Type system: `mago/crates/codex/src/ttype/`
- Metadata: `mago/crates/codex/src/metadata/`
- Pipeline: `mago/crates/orchestrator/src/service/pipeline.rs`
