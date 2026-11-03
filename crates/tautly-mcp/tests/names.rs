use tautly_mcp::names::{MAX_TOOL_NAME, assign, sanitize, tool_name};

fn ok_chars(s: &str) -> bool {
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[test]
fn names_sanitized_and_capped_at_64_with_hash() {
    assert_eq!(
        tool_name("github", "create_issue"),
        "mcp__github__create_issue"
    );
    assert_eq!(
        tool_name("my.server", "do it/now"),
        "mcp__my_server__do_it_now"
    );
    assert_eq!(sanitize("a-b_C9.é"), "a-b_C9__");
    // One `_` per character, not per byte.
    assert_eq!(tool_name("srv", "üñï"), "mcp__srv_____");

    let exactly = format!("mcp__s__{}", "t".repeat(MAX_TOOL_NAME - 8));
    assert_eq!(exactly.len(), MAX_TOOL_NAME);
    assert_eq!(tool_name("s", &"t".repeat(MAX_TOOL_NAME - 8)), exactly);

    let long_tool = "x".repeat(100);
    let name = tool_name("server", &long_tool);
    assert_eq!(name.len(), MAX_TOOL_NAME);
    assert!(ok_chars(&name), "{name}");
    let full = format!("mcp__server__{long_tool}");
    assert_eq!(&name[..57], &full[..57]);
    assert_eq!(&name[57..58], "_");
    assert!(
        name[58..]
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );
    // Stable, and different for different long names with the same first 57 characters.
    assert_eq!(name, tool_name("server", &long_tool));
    let other = tool_name("server", &format!("{}y", "x".repeat(99)));
    assert_eq!(&other[..57], &name[..57]);
    assert_ne!(other, name);
}

#[test]
fn collisions_get_distinct_names() {
    // `a.b` and `a_b` sanitize to the same name; neither keeps the plain form.
    let names = assign(&[("srv", "a.b"), ("srv", "a_b"), ("srv", "plain")]);
    assert_eq!(names.len(), 3);
    assert_eq!(names[2], "mcp__srv__plain");
    assert_ne!(names[0], names[1]);
    for n in &names[..2] {
        assert!(n.starts_with("mcp__srv__a_b_"), "{n}");
        assert!(n.len() <= MAX_TOOL_NAME && ok_chars(n), "{n}");
    }
    // The same pairs in another order get the same names.
    let swapped = assign(&[("srv", "a_b"), ("srv", "a.b"), ("srv", "plain")]);
    assert_eq!(swapped[0], names[1]);
    assert_eq!(swapped[1], names[0]);

    // Across servers too: `x__y` + `z` against `x` + `y__z`.
    let across = assign(&[("x__y", "z"), ("x", "y__z")]);
    assert_ne!(across[0], across[1]);

    // Long names that share their first 57 characters are already distinct.
    let a = "q".repeat(80);
    let b = format!("{}r", "q".repeat(79));
    let long = assign(&[("s", &a), ("s", &b)]);
    assert_eq!(long[0], tool_name("s", &a));
    assert_eq!(long[1], tool_name("s", &b));
    assert_ne!(long[0], long[1]);
}
