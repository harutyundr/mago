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
