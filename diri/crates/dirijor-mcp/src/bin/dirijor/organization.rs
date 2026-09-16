use super::*;
use diri_proto::SessionId;
use diri_proto::workspace::*;

const TIMEOUT: Duration = Duration::from_secs(10);

pub(super) fn run(scope: &str, arguments: &[String]) -> Result<(), CliError> {
    let (parsed_words, expected) = words(arguments)?;
    if scope == "workspace" && parsed_words == ["list"] && expected.is_none() {
        print_json(&request(Method::WORKSPACE_SNAPSHOT, json!({}), TIMEOUT)?);
        return Ok(());
    }
    let params = if scope == "workspace" && parsed_words == ["apply"] && expected.is_none() {
        serde_json::from_slice::<WorkspaceMutationParams>(&stdin_bytes(1_048_576, TIMEOUT))
            .map_err(|error| CliError::failure(format!("invalid workspace mutation: {error}")))?
    } else {
        let mutation = parse(scope, &parsed_words)?;
        let expected_revision = match expected {
            Some(revision) => revision,
            None => {
                let snapshot: WorkspaceSnapshot = serde_json::from_value(request(
                    Method::WORKSPACE_SNAPSHOT,
                    json!({}),
                    TIMEOUT,
                )?)
                .map_err(|error| {
                    CliError::failure(format!("invalid workspace snapshot: {error}"))
                })?;
                snapshot.revision
            }
        };
        WorkspaceMutationParams {
            expected_revision,
            mutation,
        }
    };
    print_json(&request(
        Method::WORKSPACE_MUTATE,
        serde_json::to_value(params).map_err(|error| CliError::failure(error.to_string()))?,
        TIMEOUT,
    )?);
    Ok(())
}

fn words(arguments: &[String]) -> Result<(Vec<String>, Option<u64>), CliError> {
    let mut words = Vec::new();
    let mut revision = None;
    let mut iter = arguments.iter();
    while let Some(argument) = iter.next() {
        match argument.as_str() {
            "--json" => {}
            "--" => {
                words.extend(iter.cloned());
                break;
            }
            "--revision" => {
                revision = Some(
                    iter.next()
                        .ok_or_else(|| CliError::failure("--revision needs a number"))?
                        .parse()
                        .map_err(|_| CliError::failure("invalid revision"))?,
                )
            }
            value if value.starts_with("--") => {
                return Err(CliError::failure(format!("unknown option: {value}")));
            }
            _ => words.push(argument.clone()),
        }
    }
    Ok((words, revision))
}
fn parse(scope: &str, words: &[String]) -> Result<WorkspaceMutation, CliError> {
    use WorkspaceMutation::*;
    let w = words.iter().map(String::as_str).collect::<Vec<_>>();
    let index = |value: &str| {
        value
            .parse::<usize>()
            .map_err(|_| CliError::failure("index must be a nonnegative number"))
    };
    let edge = |value| match value {
        "left" => Ok(DockEdge::Left),
        "right" => Ok(DockEdge::Right),
        "top" | "above" => Ok(DockEdge::Top),
        "bottom" | "below" => Ok(DockEdge::Bottom),
        _ => Err(CliError::failure(
            "edge must be left, right, above, or below",
        )),
    };
    let tab = |value: &str| TabId::new(value);
    let pane = |value: &str| PaneId::new(value);
    let workspace = |value: &str| WorkspaceId::new(value);
    match (scope, w.as_slice()) {
        ("workspace", ["create", name]) => Ok(CreateWorkspace {
            name: (*name).into(),
        }),
        ("workspace", ["rename", id, name]) => Ok(RenameWorkspace {
            workspace_id: workspace(id),
            name: (*name).into(),
        }),
        ("workspace", ["remove", id]) => Ok(RemoveWorkspace {
            workspace_id: workspace(id),
        }),
        ("workspace", ["move", id, destination]) => Ok(MoveWorkspace {
            workspace_id: workspace(id),
            index: index(destination)?,
        }),
        ("tab", ["create", id, session]) => Ok(CreateTab {
            select: true,
            workspace_id: workspace(id),
            session_id: SessionId::new(*session),
            title: None,
        }),
        ("tab", ["rename", id, title]) => Ok(RenameTab {
            tab_id: tab(id),
            title: Some((*title).into()),
        }),
        ("tab", ["remove", id]) => Ok(RemoveTab { tab_id: tab(id) }),
        ("tab", ["move", id, parent, destination]) => Ok(MoveTab {
            tab_id: tab(id),
            workspace_id: workspace(parent),
            index: index(destination)?,
        }),
        ("tab", ["select", parent, id]) => Ok(SelectTab {
            workspace_id: workspace(parent),
            tab_id: tab(id),
        }),
        ("pane", ["split", id, target, session, direction]) => Ok(SplitPane {
            tab_id: tab(id),
            target: pane(target),
            session_id: SessionId::new(*session),
            edge: edge(*direction)?,
        }),
        ("pane", ["remove", id, target]) => Ok(RemovePane {
            tab_id: tab(id),
            pane_id: pane(target),
        }),
        ("pane", ["move", source, id, destination, target, direction]) => Ok(MoveNode {
            source_tab: tab(source),
            node: LayoutNodeId::Pane(pane(id)),
            destination_tab: tab(destination),
            target: pane(target),
            edge: edge(*direction)?,
        }),
        ("pane", ["move-group", source, id, destination, target, direction]) => Ok(MoveNode {
            source_tab: tab(source),
            node: LayoutNodeId::Split(SplitId::new(*id)),
            destination_tab: tab(destination),
            target: pane(target),
            edge: edge(*direction)?,
        }),
        ("pane", ["swap", first_tab, first, second_tab, second]) => Ok(SwapPanes {
            first_tab: tab(first_tab),
            first: pane(first),
            second_tab: tab(second_tab),
            second: pane(second),
        }),
        ("pane", ["resize", id, split, fraction]) => Ok(ResizeSplit {
            tab_id: tab(id),
            split_id: SplitId::new(*split),
            fraction: fraction
                .parse()
                .map_err(|_| CliError::failure("fraction must be a number between 0.1 and 0.9"))?,
        }),
        ("pane", ["focus", id, target]) => Ok(FocusPane {
            tab_id: tab(id),
            pane_id: pane(target),
        }),
        ("pane", ["zoom", id, "none"]) => Ok(ZoomPane {
            tab_id: tab(id),
            pane_id: None,
        }),
        ("pane", ["zoom", id, target]) => Ok(ZoomPane {
            tab_id: tab(id),
            pane_id: Some(pane(target)),
        }),
        _ => Err(CliError::failure(
            "invalid organization command; run dirijor help for workspace/tab/pane syntax",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cli_uses_typed_mutations_and_preserves_revision() {
        let (words, revision) = words(
            &[
                "split",
                "tab_1",
                "pane_1",
                "session_remote",
                "below",
                "--revision",
                "9",
            ]
            .map(str::to_owned),
        )
        .unwrap();
        assert_eq!(revision, Some(9));
        assert!(matches!(
            parse("pane", &words).unwrap(),
            WorkspaceMutation::SplitPane {
                edge: DockEdge::Bottom,
                ..
            }
        ));
        assert!(
            parse(
                "workspace",
                &["move", "workspace_1", "-2"].map(str::to_owned)
            )
            .is_err()
        );
        assert!(
            parse(
                "pane",
                &["split", "t", "p", "s", "sideways"].map(str::to_owned)
            )
            .is_err()
        );
    }
}
