use super::*;

#[test]
fn test_louvain_two_cliques() {
    // Two 3-node cliques connected by one bridge edge
    let node_ids: Vec<String> = (0..6).map(|i| format!("n{i}")).collect();
    let node_labels: Vec<String> = vec!["Function".into(); 6];
    let id_to_idx: HashMap<String, usize> = node_ids.iter()
        .enumerate().map(|(i, id)| (id.clone(), i)).collect();

    let mut neighbors: Vec<Vec<(usize, f64)>> = vec![Vec::new(); 6];
    let edges = [(0,1), (1,2), (0,2), (3,4), (4,5), (3,5), (2,3)];
    let mut total_weight = 0.0;
    for &(a, b) in &edges {
        neighbors[a].push((b, 3.0));
        neighbors[b].push((a, 3.0));
        total_weight += 3.0;
    }

    let adj = Adjacency {
        node_ids, node_labels, id_to_idx, neighbors, total_weight,
    };
    let (mut comm, q) = louvain(&adj, 1.0);
    repair_c2(&adj, &mut comm);

    // Should find 2 communities
    let unique: HashSet<usize> = comm.iter().copied().collect();
    assert!(
        unique.len() == 2,
        "expected 2 communities, got {} (comm: {:?})",
        unique.len(), comm
    );
    // Nodes 0,1,2 in same community
    assert_eq!(comm[0], comm[1]);
    assert_eq!(comm[1], comm[2]);
    // Nodes 3,4,5 in same community
    assert_eq!(comm[3], comm[4]);
    assert_eq!(comm[4], comm[5]);
    // Different communities
    assert_ne!(comm[0], comm[3]);
    assert!(q > 0.0, "modularity should be positive, got {q}");
}

#[test]
fn test_renumber_communities() {
    let comm = vec![5, 5, 3, 3, 5, 10];
    let renumbered = renumber_communities(&comm);
    assert_eq!(renumbered[0], renumbered[1]);
    assert_eq!(renumbered[2], renumbered[3]);
    assert_eq!(renumbered[0], renumbered[4]);
    let unique: HashSet<usize> = renumbered.iter().copied().collect();
    assert_eq!(unique.len(), 3);
}
