use almighty_push::body::{BodyError, BodyMerge, ManagedBody, ManagedSection};
use almighty_push::domain::{ChangeId, LimitValues, Limits, RepositoryId};

const START: &str = "<!-- almighty-push:stack:v1:start -->";
const END: &str = "<!-- almighty-push:stack:v1:end -->";
const CHANGE: &str = "kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk";
const NEXT: &str = "llllllllllllllllllllllllllllllll";

fn limits() -> Limits {
    Limits::new(LimitValues::default()).unwrap()
}

fn change_at(index: usize) -> ChangeId {
    assert!(index < 256);
    let high = char::from_u32(u32::from(b'k') + (index / 16) as u32).unwrap();
    let low = char::from_u32(u32::from(b'k') + (index % 16) as u32).unwrap();
    ChangeId::parse(format!("{}{}{}", "k".repeat(30), high, low)).unwrap()
}

fn section() -> ManagedSection {
    ManagedSection::new(
        RepositoryId::parse("github.com/source/project").unwrap(),
        ChangeId::parse(CHANGE).unwrap(),
        vec![
            ChangeId::parse(CHANGE).unwrap(),
            ChangeId::parse(NEXT).unwrap(),
        ]
        .into_boxed_slice(),
        limits(),
    )
    .unwrap()
}

#[test]
fn managed_section_has_one_exact_versioned_meaning() {
    let section = section();
    let rendered = section.render();
    assert_eq!(
        rendered,
        format!(
            "Source repository: `github.com/source/project`\nChange ID: `{CHANGE}`\nActive stack (base to tip):\n- `{CHANGE}` (current)\n- `{NEXT}`"
        )
    );
    assert_eq!(ManagedSection::parse(&rendered, limits()).unwrap(), section);
}

#[test]
fn managed_section_rejects_invalid_stack_and_noncanonical_grammar() {
    let repository = RepositoryId::parse("github.com/source/project").unwrap();
    let current = ChangeId::parse(CHANGE).unwrap();
    assert!(matches!(
        ManagedSection::new(repository.clone(), current.clone(), Box::new([]), limits()),
        Err(BodyError::InvalidSection)
    ));
    assert!(matches!(
        ManagedSection::new(
            repository.clone(),
            current.clone(),
            vec![current.clone(), current].into_boxed_slice(),
            limits()
        ),
        Err(BodyError::InvalidSection)
    ));

    let canonical = section().render();
    for malformed in [
        canonical.replace(" (current)", ""),
        canonical.replace(&format!("- `{NEXT}`"), &format!("- `{NEXT}` (current)")),
        format!("{canonical}\ntrailing"),
        canonical.replace("Source repository", "Repository"),
        canonical.replace(CHANGE, "short"),
        canonical.replace("Active stack (base to tip):", "Active stack:"),
    ] {
        assert!(
            ManagedSection::parse(&malformed, limits()).is_err(),
            "{malformed:?}"
        );
    }
}

#[test]
fn managed_section_accepts_exact_stack_maximum_and_rejects_missing_current_or_one_more() {
    let repository = RepositoryId::parse("github.com/source/project").unwrap();
    let stack = (0..limits().change_count_max())
        .map(change_at)
        .collect::<Vec<_>>()
        .into_boxed_slice();
    let current = stack[31].clone();
    let section = ManagedSection::new(repository.clone(), current, stack, limits()).unwrap();
    assert_eq!(section.active_stack_base_to_tip().len(), 64);
    assert_eq!(
        ManagedSection::parse(&section.render(), limits()).unwrap(),
        section
    );

    assert!(matches!(
        ManagedSection::new(
            repository.clone(),
            ChangeId::parse(CHANGE).unwrap(),
            vec![ChangeId::parse(NEXT).unwrap()].into_boxed_slice(),
            limits(),
        ),
        Err(BodyError::InvalidSection)
    ));
    assert!(matches!(
        ManagedSection::new(
            repository,
            change_at(0),
            (0..=limits().change_count_max())
                .map(change_at)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            limits(),
        ),
        Err(BodyError::InvalidSection)
    ));
}

#[test]
fn strict_section_grammar_rejects_unknown_duplicate_control_and_over_bound_text() {
    let canonical = section().render();
    let duplicate_source = canonical.replacen(
        "Change ID:",
        "Source repository: `github.com/source/project`\nChange ID:",
        1,
    );
    let unknown_field = canonical.replacen("Change ID:", "Unknown: `value`\nChange ID:", 1);
    let mut malformed = vec![
        duplicate_source,
        unknown_field,
        canonical.replace("Source repository:", "Source repository:\0"),
        canonical.replace("\nChange ID:", "\r\nChange ID:"),
    ];
    malformed.push(format!(
        "{canonical}{}",
        "x".repeat(limits().body_bytes_max())
    ));
    for value in malformed {
        assert!(matches!(
            ManagedSection::parse(&value, limits()),
            Err(BodyError::InvalidSection)
        ));
    }
}

#[test]
fn append_replace_and_parse_preserve_every_unmanaged_byte() {
    let section = section();
    let BodyMerge::Changed(empty_append) = ManagedBody::merge("", &section, 4096).unwrap() else {
        panic!("new section must change an empty body")
    };
    assert_eq!(
        empty_append,
        format!("{START}\n{}\n{END}", section.render())
    );

    let appended = ManagedBody::merge("user bytes", &section, 4096).unwrap();
    let BodyMerge::Changed(appended) = appended else {
        panic!("new section must change the body")
    };
    assert_eq!(
        appended,
        format!("user bytes\n\n{START}\n{}\n{END}", section.render())
    );
    assert_eq!(
        ManagedBody::section(&appended, limits()).unwrap(),
        Some(section.clone())
    );

    let old_section = ManagedSection::new(
        RepositoryId::parse("github.com/source/project").unwrap(),
        ChangeId::parse(NEXT).unwrap(),
        vec![
            ChangeId::parse(CHANGE).unwrap(),
            ChangeId::parse(NEXT).unwrap(),
        ]
        .into_boxed_slice(),
        limits(),
    )
    .unwrap();
    for (prefix, suffix) in [
        ("prefix\r\n", "\r\nsuffix  "),
        ("λ🙂", "終\n"),
        ("", ""),
        ("no surrounding newline", "suffix-without-newline"),
    ] {
        let existing = format!("{prefix}{START}\n{}\n{END}{suffix}", old_section.render());
        let BodyMerge::Changed(replaced) = ManagedBody::merge(&existing, &section, 4096).unwrap()
        else {
            panic!("replacement must change")
        };
        let managed_start = replaced.find(START).unwrap();
        let managed_end = replaced.find(END).unwrap() + END.len();
        assert_eq!(&replaced[..managed_start], prefix);
        assert_eq!(&replaced[managed_end..], suffix);
    }
}

#[test]
fn malformed_duplicate_partial_reversed_nested_or_reserved_markers_fail_closed() {
    let section = section();
    for body in [
        START.to_owned(),
        END.to_owned(),
        format!("{END}\n{START}"),
        format!("{START}\na\n{START}\nb\n{END}"),
        format!("{START}\na\n{END}\n{END}"),
        format!("{START}\na\n{END}\n{START}\nb\n{END}"),
        "prose almighty-push:stack fragment".to_owned(),
        "<!-- almighty-push:stack:v2:start -->".to_owned(),
        format!("{START}not-newline\n{END}"),
        format!("{START}\nmanaged{END}"),
    ] {
        assert!(matches!(
            ManagedBody::merge(&body, &section, 4096),
            Err(BodyError::MalformedMarkers)
        ));
    }
}

#[test]
fn exact_body_bound_is_accepted_and_one_more_is_rejected_before_publication() {
    let section = section();
    let rendered = format!("{START}\n{}\n{END}", section.render());
    let prefix = "x".repeat(4096 - rendered.len() - 2);
    let BodyMerge::Changed(body) = ManagedBody::merge(&prefix, &section, 4096).unwrap() else {
        panic!("new body must change")
    };
    assert_eq!(body.len(), 4096);
    assert!(matches!(
        ManagedBody::merge(&(prefix + "x"), &section, 4096),
        Err(BodyError::BodyTooLarge { .. })
    ));
    assert_eq!(
        ManagedBody::merge(&body, &section, 4096).unwrap(),
        BodyMerge::Unchanged
    );
}
