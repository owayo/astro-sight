use std::collections::BTreeSet;

pub(super) struct GraphMetrics {
    pub fan_in: Vec<usize>,
    pub fan_out: Vec<usize>,
    pub depends_on: Vec<usize>,
    pub used_by: Vec<usize>,
    pub cycles: Vec<Vec<usize>>,
}

pub(super) fn analyze(count: usize, edges: &BTreeSet<(usize, usize)>) -> GraphMetrics {
    let mut outgoing = vec![Vec::new(); count];
    let mut incoming = vec![Vec::new(); count];
    for &(from, to) in edges {
        outgoing[from].push(to);
        incoming[to].push(from);
    }
    // Kosaraju の両パスを明示スタックで行う。全到達行列は保持しない。
    let mut seen = vec![false; count];
    let mut order = Vec::with_capacity(count);
    for start in 0..count {
        if seen[start] {
            continue;
        }
        seen[start] = true;
        let mut pending = vec![(start, 0)];
        while let Some((node, next)) = pending.last_mut() {
            if let Some(&child) = outgoing[*node].get(*next) {
                *next += 1;
                if !seen[child] {
                    seen[child] = true;
                    pending.push((child, 0));
                }
            } else {
                order.push(*node);
                pending.pop();
            }
        }
    }
    let mut owner = vec![usize::MAX; count];
    let mut groups = Vec::new();
    for start in order.into_iter().rev() {
        if owner[start] != usize::MAX {
            continue;
        }
        let id = groups.len();
        let mut group = Vec::new();
        let mut pending = vec![start];
        owner[start] = id;
        while let Some(node) = pending.pop() {
            group.push(node);
            for &parent in &incoming[node] {
                if owner[parent] == usize::MAX {
                    owner[parent] = id;
                    pending.push(parent);
                }
            }
        }
        group.sort_unstable();
        groups.push(group);
    }
    let mut dag = vec![BTreeSet::new(); groups.len()];
    let mut reverse = vec![BTreeSet::new(); groups.len()];
    for &(from, to) in edges {
        let (a, b) = (owner[from], owner[to]);
        if a != b {
            dag[a].insert(b);
            reverse[b].insert(a);
        }
    }
    let sizes: Vec<usize> = groups.iter().map(Vec::len).collect();
    let forward = reachable_counts(&dag, &sizes);
    let backward = reachable_counts(&reverse, &sizes);
    let mut cycles: Vec<_> = groups.into_iter().filter(|g| g.len() > 1).collect();
    cycles.sort();
    GraphMetrics {
        fan_in: incoming.iter().map(Vec::len).collect(),
        fan_out: outgoing.iter().map(Vec::len).collect(),
        depends_on: owner.iter().map(|&id| forward[id]).collect(),
        used_by: owner.iter().map(|&id| backward[id]).collect(),
        cycles,
    }
}

fn reachable_counts(graph: &[BTreeSet<usize>], sizes: &[usize]) -> Vec<usize> {
    let mut seen = vec![usize::MAX; graph.len()];
    let mut counts = vec![0; graph.len()];
    let mut pending = Vec::new();
    for (start, count) in counts.iter_mut().enumerate() {
        seen[start] = start;
        pending.push(start);
        while let Some(node) = pending.pop() {
            *count += sizes[node];
            for &child in &graph[node] {
                if seen[child] != start {
                    seen[child] = start;
                    pending.push(child);
                }
            }
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_diamond_cycle_and_isolated() {
        let edges = BTreeSet::from([(0, 1), (0, 2), (1, 3), (2, 3), (3, 1)]);
        let result = analyze(5, &edges);
        assert_eq!(result.depends_on, [4, 2, 3, 2, 1]);
        assert_eq!(result.used_by, [1, 4, 2, 4, 1]);
        assert_eq!(result.cycles, [vec![1, 3]]);
        assert_eq!(result.fan_in, [0, 2, 1, 2, 0]);
        assert!(analyze(0, &BTreeSet::new()).cycles.is_empty());
    }

    #[test]
    fn graph_deep_cycle_uses_no_recursion() {
        let count = 20_000;
        let edges = (0..count).map(|i| (i, (i + 1) % count)).collect();
        let result = analyze(count, &edges);
        assert_eq!(result.cycles.len(), 1);
        assert_eq!(result.depends_on, vec![count; count]);
        assert_eq!(result.used_by, vec![count; count]);
    }

    #[test]
    fn graph_matches_independent_matrix_oracle_for_all_four_node_graphs() {
        let possible: Vec<_> = (0..4)
            .flat_map(|a| (0..4).filter(move |&b| a != b).map(move |b| (a, b)))
            .collect();
        for mask in 0..(1_u32 << possible.len()) {
            let edges: BTreeSet<_> = possible
                .iter()
                .enumerate()
                .filter_map(|(bit, &edge)| (mask & (1 << bit) != 0).then_some(edge))
                .collect();
            let result = analyze(4, &edges);
            let mut reachable = [[false; 4]; 4];
            for (id, row) in reachable.iter_mut().enumerate() {
                row[id] = true;
            }
            for &(a, b) in &edges {
                reachable[a][b] = true;
            }
            for via in 0..4 {
                for from in 0..4 {
                    for to in 0..4 {
                        reachable[from][to] |= reachable[from][via] && reachable[via][to];
                    }
                }
            }
            for a in 0..4 {
                assert_eq!(
                    result.depends_on[a],
                    reachable[a].iter().filter(|&&v| v).count()
                );
                assert_eq!(
                    result.used_by[a],
                    reachable.iter().filter(|row| row[a]).count()
                );
                for (b, row) in reachable.iter().enumerate() {
                    if a != b {
                        assert_eq!(
                            result
                                .cycles
                                .iter()
                                .any(|group| group.contains(&a) && group.contains(&b)),
                            reachable[a][b] && row[a],
                            "mask={mask}, pair=({a},{b})"
                        );
                    }
                }
            }
        }
    }
}
