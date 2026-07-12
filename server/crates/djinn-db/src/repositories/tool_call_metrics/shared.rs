use crate::repositories::tool_call_export::NormalizedToolCallRow;

/// Group row indices by `(session_id, task_id)` preserving input order.
pub fn group_by_session_task(rows: &[NormalizedToolCallRow]) -> Vec<Vec<usize>> {
    let mut groups: Vec<(String, Option<String>, Vec<usize>)> = Vec::new();
    'outer: for (i, row) in rows.iter().enumerate() {
        for g in &mut groups {
            if g.0 == row.session_id && g.1 == row.task_id {
                g.2.push(i);
                continue 'outer;
            }
        }
        groups.push((row.session_id.clone(), row.task_id.clone(), vec![i]));
    }
    groups.into_iter().map(|g| g.2).collect()
}
