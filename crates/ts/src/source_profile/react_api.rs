//! React API usage detection.
//!
//! Detects usage of React APIs that have behavioral implications when
//! added or removed between versions:
//!
//! - `ReactDOM.createPortal()` — renders content outside component's DOM tree
//! - `useContext(X)` — component depends on a Context provider
//! - `React.forwardRef()` — component forwards refs
//! - `React.memo()` — component memoizes render output

/// React API usage detected in a component's source.
#[derive(Debug, Clone, Default)]
pub struct ReactApiUsage {
    /// Component uses `createPortal` (from `react-dom`).
    pub uses_portal: bool,

    /// Target expression for `createPortal`, if statically extractable.
    /// e.g., "document.body", "this.getElement(appendTo)"
    pub portal_target: Option<String>,

    /// Context names consumed via `useContext(X)`.
    pub consumed_contexts: Vec<String>,

    /// Component is wrapped in `forwardRef`.
    pub is_forward_ref: bool,

    /// Component is wrapped in `memo`.
    pub is_memo: bool,
}

/// Extract React API usage from source text.
///
/// This does a simple text scan for well-known patterns rather than full
/// AST analysis — these patterns are distinctive enough that regex/string
/// matching is reliable and much faster.
pub fn detect_react_api_usage(source: &str) -> ReactApiUsage {
    let mut usage = ReactApiUsage::default();

    // createPortal detection
    // Patterns: ReactDOM.createPortal(, createPortal(
    if source.contains("createPortal(") || source.contains("createPortal (") {
        usage.uses_portal = true;

        // Try to extract the second argument (target container)
        // createPortal(content, target) — find the target
        if let Some(target) = extract_portal_target(source) {
            usage.portal_target = Some(target);
        }
    }

    // useContext detection
    // Pattern: useContext(SomeName)
    extract_use_context_calls(source, &mut usage.consumed_contexts);

    // forwardRef detection
    // Patterns: forwardRef(, React.forwardRef(, forwardRef<
    if source.contains("forwardRef(") || source.contains("forwardRef<") {
        usage.is_forward_ref = true;
    }

    // memo detection
    // Patterns: memo(, React.memo(
    // Be careful not to match "memo" as part of other words
    for (i, _) in source.match_indices("memo(") {
        if is_memo_call(source, i) {
            usage.is_memo = true;
            break;
        }
    }

    usage
}

/// Extract the target container argument from a `createPortal(content, target)` call.
fn extract_portal_target(source: &str) -> Option<String> {
    // Find createPortal( and then extract the second argument
    let portal_idx = source.find("createPortal(")?;
    let after = &source[portal_idx + "createPortal(".len()..];

    // Simple approach: count parens to find the comma separating args
    let mut depth = 1;
    let mut comma_pos = None;
    for (i, ch) in after.char_indices() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            ',' if depth == 1 => {
                comma_pos = Some(i);
                break;
            }
            _ => {}
        }
    }

    let comma = comma_pos?;
    let target_start = comma + 1;
    let rest = &after[target_start..];

    // Find the end of the second argument
    let mut depth = 1;
    let mut end = 0;
    for (i, ch) in rest.char_indices() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    end = i;
                    break;
                }
            }
            ',' if depth == 1 => {
                end = i;
                break;
            }
            _ => {}
        }
    }

    let target = rest[..end].trim().to_string();
    if target.is_empty() {
        None
    } else {
        Some(target)
    }
}

/// Extract `useContext(X)` calls, collecting context names.
fn extract_use_context_calls(source: &str, contexts: &mut Vec<String>) {
    let pattern = "useContext(";
    for (idx, _) in source.match_indices(pattern) {
        let after = &source[idx + pattern.len()..];

        // Read the argument — should be an identifier
        let trimmed = after.trim_start();
        let name_end = trimmed
            .find(|c: char| !c.is_alphanumeric() && c != '_' && c != '$')
            .unwrap_or(trimmed.len());

        if name_end > 0 {
            let name = trimmed[..name_end].to_string();
            if !contexts.contains(&name) {
                contexts.push(name);
            }
        }
    }
}

/// Check that a `memo(` match is actually a `React.memo(` or standalone `memo(` call,
/// not part of another word like "memorize(" or "memoize(".
fn is_memo_call(source: &str, idx: usize) -> bool {
    if idx == 0 {
        return true;
    }
    let prev = source.as_bytes()[idx - 1];
    // Valid preceding characters for a `memo(` call: whitespace, `.`, `(`, `=`, `,`, `;`, `{`, `[`
    // Invalid: alphanumeric or `_` (part of a larger identifier)
    !prev.is_ascii_alphanumeric() && prev != b'_' && prev != b'$'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_portal() {
        let source = r#"
            return ReactDOM.createPortal(
                <ModalContent>{children}</ModalContent>,
                this.getElement(appendTo)
            );
        "#;
        let usage = detect_react_api_usage(source);
        assert!(usage.uses_portal);
        assert_eq!(
            usage.portal_target,
            Some("this.getElement(appendTo)".into())
        );
    }

    #[test]
    fn test_detect_use_context() {
        let source = r#"
            const { isExpanded } = useContext(AccordionItemContext);
            const theme = useContext(ThemeContext);
        "#;
        let usage = detect_react_api_usage(source);
        assert_eq!(
            usage.consumed_contexts,
            vec!["AccordionItemContext", "ThemeContext"]
        );
    }

    #[test]
    fn test_detect_forward_ref() {
        let source = r#"
            export const Dropdown = forwardRef((props: DropdownProps, ref: React.Ref<any>) => (
                <DropdownBase innerRef={ref} {...props} />
            ));
        "#;
        let usage = detect_react_api_usage(source);
        assert!(usage.is_forward_ref);
    }

    #[test]
    fn test_detect_memo() {
        let source = r#"
            export const MyComponent = React.memo(function MyComponent(props) {
                return <div>{props.children}</div>;
            });
        "#;
        let usage = detect_react_api_usage(source);
        assert!(usage.is_memo);
    }

    #[test]
    fn test_no_false_memo_match() {
        let source = r#"
            const memoize = (fn) => { /* ... */ };
            const memorize = () => {};
        "#;
        let usage = detect_react_api_usage(source);
        assert!(!usage.is_memo);
    }

    #[test]
    fn test_no_portal() {
        let source = r#"
            return <div>{children}</div>;
        "#;
        let usage = detect_react_api_usage(source);
        assert!(!usage.uses_portal);
        assert!(usage.portal_target.is_none());
    }
}
