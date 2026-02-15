# Return Type Inference Implementation Guide

## Overview

This document describes the return type inference improvements implemented for mago to better handle XenForo framework code, which lacks comprehensive type hints. The implementation follows mago's 3-phase architecture and adds both simple (scan-phase) and hint-based (populate-phase) type inference.

## Architecture Background

Mago uses a 3-phase analysis pipeline:

1. **Scan Phase** (per-file, parallel):
   - No codebase access
   - Collects metadata per file
   - Performs simple type inference (literals, `$this`, `new ClassName()`, variable assignments)
   - Collects "hints" for complex cases requiring codebase access

2. **Populate Phase** (sequential, full codebase):
   - Resolves hints collected during scan
   - Performs inheritance lookups
   - Uses fixed-point iteration for recursive type resolution

3. **Analyze Phase** (parallel, read-only):
   - Reports errors based on resolved types

## Problem Statement

XenForo framework code often lacks explicit return type hints, causing two main issues:

1. **Empty arrays typed incorrectly**: `[]` was inferred as `list{}` (non-empty list) instead of `array<array-key, mixed>`
2. **Method chains return `mixed`**: Calls like `\XF::app()->language()->dateTime()` couldn't be resolved
3. **Variable-based returns not handled**: Common pattern `$x = new Foo(); return $x;` failed to infer type

## Implementation Details

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

**Rationale**: Empty arrays should be typed as generic arrays, not non-empty lists, to match common PHP usage patterns.

### 2. Return Expression Hints

**File**: `mago/crates/codex/src/metadata/function_like.rs`

**Addition**: Lines 142-160 - New `ReturnExpressionHint` enum

```rust
pub enum ReturnExpressionHint {
    InstanceMethodCall { class: Atom, method: Atom },  // $this->method()
    StaticMethodCall { class: Atom, method: Atom },     // ClassName::method()
    FunctionCall { function: Atom },                    // function()
    MethodChain { receiver_class: Atom, methods: Box<[Atom]> },  // $this->a()->b()
}
```

These hints are collected during scan phase when a return expression is too complex to resolve without codebase access.

### 3. Hint Collection During Scan

**File**: `mago/crates/codex/src/scanner/function_like.rs`

**Key Functions**:

- `collect_return_expression_hints()` - Finds all return statements and extracts hints
- `extract_return_hint()` - Converts a return expression to a hint
- `extract_method_chain_from_expression()` - Recursively builds method chains

**Important Details**:

- Class names are **lowercased** using `ascii_lowercase_atom()` for consistent lookups
- Fully qualified names have leading `\` stripped
- Function names are stored without namespace prefix for fallback resolution
- Variable-based returns (`return $var`) are resolved by finding the last assignment to that variable in the same block

**Example**:
```php
public function getDateTime() {
    return $this->language()->dateTime();
}
```

Becomes hint: `MethodChain { receiver_class: "xf\app", methods: ["language", "datetime"] }`

### 4. Variable Assignment Tracking

**File**: `mago/crates/codex/src/scanner/function_like.rs`

**New Function**: `find_last_assignment_in_block()`

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

**AST Pattern**: 
```
Statement::Expression(
    ExpressionStatement {
        expression: Expression::Assignment(
            Assignment {
                lhs: Expression::Variable(Variable::Direct(DirectVariable { name })),
                operator: AssignmentOperator::Assign(_),
                rhs: <expression>
            }
        )
    }
)
```

**Usage in `infer_atomic_from_expression()`**:
```rust
Expression::Variable(Variable::Direct(direct)) if !direct.name.eq_ignore_ascii_case("$this") => {
    let block = block?;
    let assigned_expr = find_last_assignment_in_block(block, direct.name)?;
    infer_atomic_from_expression(assigned_expr, classname, context, None)
}
```

This handles patterns like:
```php
public function addColumn($name, $type) {
    $column = new Column($name, $type);
    // ... operations on $column ...
    return $column;  // Now correctly inferred as Column type
}
```

**Usage in `extract_return_hint()`**:
```rust
Expression::Variable(Variable::Direct(direct)) if !direct.name.eq_ignore_ascii_case("$this") => {
    let block = block?;
    let assigned_expr = find_last_assignment_in_block(block, direct.name)?;
    extract_return_hint(assigned_expr, current_classname, context, None)
}
```

This handles patterns like:
```php
public function getResult() {
    $result = $this->calculate();
    return $result;  // Creates hint for $this->calculate()
}
```

### 5. Simple Type Inference During Scan

**File**: `mago/crates/codex/src/scanner/function_like.rs`

**Function**: `infer_atomic_from_expression()`

Handles immediate cases without needing codebase access:

- `$this` → `static` (TNamedObject with is_this=true)
- `true`/`false` → `bool`
- `123` → `int`
- `1.5` → `float`
- `'string'` → `string`
- `null` → `null`
- `new ClassName()` → `ClassName` (resolved via context)
- `$variable` → type of last assignment (new feature)

### 6. Hint Resolution During Populate

**File**: `mago/crates/codex/src/populator/return_hints.rs` (NEW FILE, 285 lines)

**Main Function**: `resolve_return_expression_hints(codebase: &mut CodebaseMetadata)`

Uses **fixed-point iteration** to resolve hints until no more progress can be made:

```rust
loop {
    let mut changed = false;
    
    for each function/method with unresolved hints {
        if can resolve hint {
            set return type
            changed = true
        }
    }
    
    if !changed { break; }
}
```

**Key Resolution Functions**:

1. **`resolve_function_return()`** - Resolves global function calls
   - First tries exact name match
   - Falls back to stripping namespace prefix (e.g., `xf\strtr` → `strtr`)
   
2. **`resolve_method_return()`** - Resolves instance/static method calls
   - Looks up method in class
   - If not found, searches parent classes via `all_parent_classes`
   - If still not found, searches traits via `used_traits`
   - Always lowercases class names for lookup
   
3. **`resolve_method_chain()`** - Resolves chained method calls
   - Recursively walks the chain: `a()->b()->c()`
   - Each step returns the type for the next step's receiver
   - Uses `resolve_method_return()` for each link

4. **`extract_class_from_type()`** - Extracts class name from TUnion
   - Handles `TObject::Named(TNamedObject)`
   - Always **lowercases** the class name
   - Critical for consistent lookups

**Important Note**: Class names in mago's metadata are stored **lowercase**. All lookups must use `ascii_lowercase_atom()` or they will fail.

### 7. Integration

**File**: `mago/crates/codex/src/populator/mod.rs`

**Addition**: Line 169

```rust
// Resolve return types from collected hints
return_hints::resolve_return_expression_hints(codebase);
```

This runs after initial metadata collection but before analysis, allowing resolved types to be available for error checking.

## PHP Namespace Resolution Rules

PHP has special resolution rules for unqualified function calls:

```php
namespace XF;

function test() {
    strtr(...);  // First tries \XF\strtr, falls back to \strtr
}
```

Our implementation handles this by:
1. Storing function names with full namespace during scan
2. During populate, trying exact match first
3. Falling back to global namespace (strip everything before last `\`)

## Class Name Normalization

PHP class names are case-insensitive. Mago stores them **lowercase** internally:

```rust
// ❌ WRONG - Will fail lookup
let class_atom = atom("XF\\App");

// ✅ CORRECT - Will succeed
let class_atom = ascii_lowercase_atom("XF\\App");  // becomes "xf\\app"
```

**Everywhere class names are used**:
- Hint extraction during scan
- Hint resolution during populate
- Class lookups in metadata maps

## Testing

Build in debug mode (much faster than release):
```bash
cd mago
cargo build
```

Test specific file:
```bash
./mago/target/debug/mago analyze path/to/file.php
```

Test full project:
```bash
./mago/target/debug/mago analyze
```

## Results

Before implementation:
- 9 issues in test files (empty array errors, method chain errors)

After implementation:
- 0 issues in target test files (Setup.php, ScanResult.php)
- No regressions in other files

## Future Enhancements

Potential areas for improvement:

1. **Property type inference**: Track property assignments across methods
2. **Parameter type inference**: Infer from usage patterns
3. **More complex control flow**: Handle if/else branches, loops
4. **Docblock type parsing**: Fall back to @return annotations when inference fails
5. **Cross-file variable tracking**: Track variables across method boundaries
6. **Array element types**: Infer element types from array operations

## Key Learnings

1. **Separate concerns**: Simple inference in scan phase, complex in populate phase
2. **Case sensitivity**: Always lowercase class names, case-insensitive variable names
3. **Namespace handling**: Functions need fallback, classes don't
4. **Inheritance**: Check parent classes AND traits for methods
5. **Fixed-point iteration**: Required for recursive/circular type dependencies
6. **AST patterns**: Use pattern matching to extract semantic information
7. **Last assignment wins**: Simple linear scan works for most cases
8. **Avoid recursion loops**: Pass `None` for block when recursing to prevent infinite loops

## Files Modified

1. `mago/crates/codex/src/scanner/inference/mod.rs` - Empty array fix
2. `mago/crates/codex/src/metadata/function_like.rs` - ReturnExpressionHint enum
3. `mago/crates/codex/src/scanner/function_like.rs` - Hint collection, variable tracking, and property name fix
4. `mago/crates/codex/src/populator/return_hints.rs` - NEW FILE for hint resolution
5. `mago/crates/codex/src/populator/mod.rs` - Integration point
6. `mago/crates/codex/src/scanner/property.rs` - Property inference from defaults (pre-existing)
7. `mago/crates/codex/src/scanner/class_like.rs` - Enum method constructors (pre-existing)

## Bug Fixes (Feb 15, 2026)

**Property Access Hint Bug**: Fixed property name mismatch in `PropertyAccess` hints. Property metadata stores names with `$` prefix (e.g., `"$post"`), but hints were storing them without (e.g., `"post"`). This caused all property-based return type inference to fail.

**Fix Location**: `mago/crates/codex/src/scanner/function_like.rs`, line 942-949
- Changed property name extraction to include `$` prefix: `format!("${}", ident.value)`
- This matches how property names are stored in class metadata

**Impact**:
- Fixed return type inference for methods like `getPost()` that return `$this->property`
- Reduced `mixed-assignment` warnings from 12 to 9 in BHW_OriginalityApi project
- Specifically fixed 3 warnings in `PreparerService.php` at lines 52, 169, 291

## References

- Mago AST types: `mago/crates/syntax/src/ast/ast/`
- Type system: `mago/crates/codex/src/ttype/`
- Metadata: `mago/crates/codex/src/metadata/`
- Pipeline: `mago/crates/orchestrator/src/service/pipeline.rs`

# Mago Type Inference Fix - Summary

## Date: February 15, 2026

## Problem

Property-based return type inference was failing, causing `unknown-ref(XF\Entity\Post)` errors throughout the codebase. Methods like `getPost()` that return `$this->property` were being typed as `unknown-ref` instead of concrete object types.

### Example Error
```
error[invalid-property-access]: Attempting to access a property on a non-object type (`unknown-ref(XF\Entity\Post)`).
   ┌─ upload/src/addons/BHW/OriginalityApi/XF/Service/Post/PreparerService.php:56:26
   │
56 │         $thread = $post->Thread;
   │                   -----  ^^^^^^ Cannot access property here
   │                   │
   │                   This expression has type `unknown-ref(XF\Entity\Post)` 
```

---

## Root Cause

The `resolve_return_expression_hints` function in the populate phase runs **before** class-like type population. At that point, property types stored in metadata are still `TReference::Symbol` (unresolved references), not `TObject::Named` (concrete object types).

### Pipeline Order Issue
1. Line 136-167: Function-like signatures populated
2. **Line 170: `resolve_return_expression_hints` runs** ← Our code runs here
3. Line 184-200: Class-like types populated (TReference → TObject conversion)

When our hint resolution reads property types, they're still unresolved `TReference::Symbol` types.

---

## Solution

Modified `collect_atomics()` function in `return_hints.rs` to convert `TReference::Symbol` to `TObject::Named` when assembling resolved return types. This is the single funnel point where all resolved types flow through.

### Code Change

**File:** `mago/crates/codex/src/populator/return_hints.rs`

**Lines 297-321:**
```rust
fn collect_atomics(union: &TUnion, atomics: &mut Vec<TAtomic>) {
    for atomic in union.types.iter() {
        // Resolve TReference::Symbol to TObject::Named so the analyzer sees concrete object types
        // instead of unresolved references (which display as "unknown-ref(ClassName)").
        // This is needed because resolve_return_expression_hints runs before class-like type
        // population, so property/method return types may still contain TReference::Symbol.
        let resolved = match atomic {
            TAtomic::Reference(TReference::Symbol { name, parameters, intersection_types }) => {
                let mut named = TNamedObject::new(*name);
                if let Some(params) = parameters {
                    named = named.with_type_parameters(Some(params.clone()));
                }
                if let Some(intersections) = intersection_types {
                    for it in intersections {
                        named.add_intersection_type(it.clone());
                    }
                }
                TAtomic::Object(TObject::Named(named))
            }
            other => other.clone(),
        };
        if !atomics.contains(&resolved) {
            atomics.push(resolved);
        }
    }
}
```

**Imports Added:**
```rust
use crate::ttype::TType;
use crate::ttype::atomic::object::named::TNamedObject;
```

---

## Impact

### Before Fix
- **PreparerService.php:** 23 issues (16 errors including multiple `unknown-ref` errors)
- **Full Project:** 64 issues (27 errors)
- Multiple `invalid-property-access` errors on XF types

### After Fix
- **PreparerService.php:** 0 issues ✅
- **Full Project:** 41 issues (11 errors) - **16 fewer errors**
- **Zero `unknown-ref` errors** ✅
- **Zero `invalid-property-access` on XF types** ✅

---

## Why This Approach Works

1. **Single Point of Resolution:** All resolved types flow through `collect_atomics()`, so one fix handles:
    - Property access (`$this->property`)
    - Method calls (`$this->method()`)
    - Method chains (`$this->a()->b()->c()`)

2. **Preserves Type Information:** Converts references while maintaining:
    - Generic type parameters
    - Intersection types
    - Class names

3. **No Pipeline Changes:** Doesn't require reordering the populate pipeline, which could have unintended side effects

4. **Analysis-Time Solution:** Resolves types at the point they're used for return type inference, not earlier in the pipeline

---

## Files Modified

**Single File Changed:**
- `mago/crates/codex/src/populator/return_hints.rs`
    - Modified `collect_atomics()` function (lines 297-321)
    - Added 2 imports (lines 12, 14)

**No Changes Needed To:**
- Pipeline ordering
- Property scanning
- Method resolution
- Analysis phase

---

## Testing

### Build
```bash
cd /Users/harutyun/Projects/XenForo2/BHW/BHW_OriginalityApi/mago
/Users/harutyun/.cargo/bin/cargo build
```

### Test Single File
```bash
cd /Users/harutyun/Projects/XenForo2/BHW/BHW_OriginalityApi
./mago/target/debug/mago analyse upload/src/addons/BHW/OriginalityApi/XF/Service/Post/PreparerService.php
```

**Result:** ✅ No issues found

### Test Full Project
```bash
./mago/target/debug/mago analyse
```

**Result:** ✅ Zero `unknown-ref` errors, 16 fewer errors overall

---

## Key Learnings

1. **TReference vs TObject:** `TReference::Symbol` is an unresolved reference that displays as `unknown-ref(ClassName)`. It must be converted to `TObject::Named` for the analyzer to recognize it as an object type.

2. **Pipeline Timing:** Understanding when different phases run is critical. Our hint resolution runs before type population, so we must handle unresolved references ourselves.

3. **Funnel Points:** Finding the single point where all data flows through (like `collect_atomics()`) allows fixing multiple issues with one change.

4. **Type Preservation:** When converting types, preserve all metadata (generics, intersections) to maintain type safety.

---

## Related Documentation

- Original implementation guide: `RETURN_TYPE_INFERENCE_IMPLEMENTATION.md`
- Previous work output: `PMOMPT.md` (deleted after completion)
- Code improvements: `../CODE_IMPROVEMENTS.md`

---

## Verification Commands

```bash
# Check for any remaining unknown-ref errors
./mago/target/debug/mago analyse 2>&1 | grep "unknown-ref"

# Check for invalid-property-access on XF types
./mago/target/debug/mago analyse 2>&1 | grep "invalid-property-access.*XF"

# Full analysis
./mago/target/debug/mago analyse
```

All should return zero results for type inference issues with XF classes.

## Bug Fixes (Feb 15, 2026 - Part 2)

### Fix 1: Static/Self/Parent Keyword AST Handling

**Problem**: Static method calls using `static::`, `self::`, or `parent::` were not generating return expression hints. Methods like `\XF::phrase()` (which internally calls `static::language()->phrase(...)`) returned `mixed`.

**Root Cause**: The PHP parser represents `static`, `self`, and `parent` keywords as dedicated AST expression types (`Expression::Static`, `Expression::Self_`, `Expression::Parent`), NOT as `Expression::Identifier(Identifier::Local("static"))`. The hint extraction code only handled the `Identifier::Local` case, which never matched.

**Fix Locations**:
- `mago/crates/codex/src/scanner/function_like.rs` — `extract_return_hint()` (StaticMethodCall case)
- `mago/crates/codex/src/scanner/function_like.rs` — `extract_method_chain_from_expression()` (StaticMethodCall case)

**Changes**: Added `Expression::Static(_)`, `Expression::Self_(_)`, and `Expression::Parent(_)` match arms in both functions, mapping them to `current_classname` (same behavior as the old `Identifier::Local` guard that checked for "static"/"self").

### Fix 2: Property Name Case Sensitivity

**Problem**: `$addOn->getInstalledAddOn()` returned `mixed` even though the method body is `return $this->installedAddOn;` and the property has a `@var \XF\Entity\AddOn` docblock.

**Root Cause**: Property names in hints were lowercased via `ascii_lowercase_atom()` (e.g., `$installedaddon`), but property metadata stores them with original casing (`$installedAddOn`). PHP property names are **case-sensitive**, so the lookup failed.

**Fix Location**: `mago/crates/codex/src/scanner/function_like.rs` — `extract_return_hint()` (PropertyAccess case, line ~954)

**Change**: Replaced `ascii_lowercase_atom(&name_with_dollar)` with `atom(&name_with_dollar)` to preserve original property name casing.

### Impact

- **Before**: 41 issues (11 errors) in full project
- **After**: 13 issues (3 errors, 10 warnings) — **28 fewer issues**
- **ErrorLog.php**: 0 issues (was 1 error) ✅
- **IntegrationTest.php**: 0 issues (was 2 errors + 1 warning) ✅
- **Zero regressions** ✅

### Key Learnings

1. **AST keyword representation**: PHP keywords like `static`, `self`, `parent` are parsed as dedicated expression types, not identifiers. Always check the actual AST representation.
2. **Case sensitivity matters**: Class/method names are case-insensitive in PHP (use `ascii_lowercase_atom`), but property names are case-sensitive (use `atom`).
