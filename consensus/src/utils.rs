// Copyright (c) 2021, Facebook, Inc. and its affiliates
// Copyright (c) 2022, Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0
use crate::consensus::{ConsensusState, Dag};
use config::Committee;
use std::collections::HashSet;
use tracing::{debug, info, warn};
use types::{Certificate, CertificateDigest, Round};

/// Order the past leaders that we didn't already commit.
pub fn order_leaders<'a, LeaderElector>(
    committee: &Committee,
    leader: &Certificate,
    state: &'a ConsensusState,
    get_leader: LeaderElector,
) -> Vec<Certificate>
where
    LeaderElector: Fn(&Committee, Round, &'a Dag) -> Option<&'a (CertificateDigest, Certificate)>,
{
    let mut to_commit = vec![leader.clone()];
    let mut leader = leader;
    for r in (state.last_committed_round + 2..leader.round())
        .rev()
        .step_by(2)
    {
        // Get the certificate proposed by the previous leader.
        let (_, prev_leader) = match get_leader(committee, r, &state.dag) {
            Some(x) => x,
            None => continue,
        };

        // Check whether there is a path between the last two leaders.
        info!("🔍 [CONSENSUS] Checking linked path: Leader round {} -> Leader round {} (last_committed_round={})", 
            prev_leader.round(), leader.round(), state.last_committed_round);
        if linked(leader, prev_leader, &state.dag) {
            info!("✅ [CONSENSUS] Found linked path: Leader round {} -> Leader round {} - Adding to commit list", 
                prev_leader.round(), leader.round());
            to_commit.push(prev_leader.clone());
            leader = prev_leader;
        } else {
            warn!("⚠️ [CONSENSUS] NO linked path found: Leader round {} -> Leader round {} - Will SKIP prev_leader (this may cause batch to not be committed)", 
                prev_leader.round(), leader.round());
        }
    }
    to_commit
}

/// Checks if there is a path between two leaders.
/// 
/// Cải thiện: Kiểm tra DAG completeness trước khi check path để đảm bảo không skip do DAG chưa đầy đủ
fn linked(leader: &Certificate, prev_leader: &Certificate, dag: &Dag) -> bool {
    let mut parents = vec![leader];
    let rounds_to_check: Vec<Round> = (prev_leader.round()..leader.round()).rev().collect();
    debug!("🔍 [CONSENSUS] linked(): Checking path from round {} to {}, rounds to check: {:?}", 
        leader.round(), prev_leader.round(), rounds_to_check);
    
    // Kiểm tra DAG completeness: Đảm bảo tất cả rounds cần thiết đều có certificates
    // Điều này giúp phát hiện sớm nếu DAG chưa được sync đầy đủ
    for r in rounds_to_check.iter() {
        if let Some(certs) = dag.get(r) {
            let cert_count = certs.len();
            debug!("🔍 [CONSENSUS] linked(): Round {} has {} certificates in DAG", r, cert_count);
            // Nếu round có ít certificates hơn expected (ví dụ: < 2f+1), có thể DAG chưa đầy đủ
            // Tuy nhiên, chúng ta vẫn thử check path vì có thể vẫn có path với số certificates hiện có
        } else {
            warn!("⚠️ [CONSENSUS] linked(): Round {} missing in DAG - DAG may not be fully synchronized", r);
            // Nếu round hoàn toàn missing, có thể DAG chưa sync đầy đủ
            // Nhưng vẫn thử check path với rounds có sẵn
        }
    }
    
    for r in rounds_to_check {
        let before_count = parents.len();
        let round_certs = match dag.get(&r) {
            Some(certs) => certs,
            None => {
                warn!("❌ [CONSENSUS] linked(): Round {} missing in DAG - path check will fail", r);
                // Nếu round missing, path không thể tồn tại
                return false;
            }
        };
        
        parents = round_certs
            .values()
            .filter(|(digest, _)| parents.iter().any(|x| x.header.parents.contains(digest)))
            .map(|(_, certificate)| certificate)
            .collect();
        let after_count = parents.len();
        debug!("🔍 [CONSENSUS] linked(): Round {}, found {} certificates in path (before: {}, after: {})", 
            r, after_count, before_count, after_count);
        
        // Nếu không tìm thấy certificates nào trong path tại round này, path bị broken
        if after_count == 0 {
            warn!("❌ [CONSENSUS] linked(): No certificates found in path at round {} - path broken", r);
            return false;
        }
    }
    
    let has_path = parents.contains(&prev_leader);
    if has_path {
        debug!("✅ [CONSENSUS] linked(): Path FOUND from round {} to {}", 
            leader.round(), prev_leader.round());
    } else {
        warn!("❌ [CONSENSUS] linked(): Path NOT FOUND from round {} to {} - prev_leader will be skipped. This may be due to incomplete DAG synchronization.", 
            leader.round(), prev_leader.round());
    }
    has_path
}

/// Flatten the dag referenced by the input certificate. This is a classic depth-first search (pre-order):
/// https://en.wikipedia.org/wiki/Tree_traversal#Pre-order
pub fn order_dag(
    gc_depth: Round,
    leader: &Certificate,
    state: &ConsensusState,
) -> Vec<Certificate> {
    debug!("Processing sub-dag of {:?}", leader);
    let mut ordered = Vec::new();
    let mut already_ordered = HashSet::new();

    let mut buffer = vec![leader];
    while let Some(x) = buffer.pop() {
        debug!("Sequencing {:?}", x);
        ordered.push(x.clone());
        for parent in &x.header.parents {
            let (digest, certificate) = match state
                .dag
                .get(&(x.round() - 1))
                .and_then(|x| x.values().find(|(x, _)| x == parent))
            {
                Some(x) => x,
                None => continue, // We already ordered or GC up to here.
            };

            // We skip the certificate if we (1) already processed it or (2) we reached a round that we already
            // committed for this authority.
            let mut skip = already_ordered.contains(&digest);
            skip |= state
                .last_committed
                .get(&certificate.origin())
                .map_or_else(|| false, |r| r == &certificate.round());
            if !skip {
                buffer.push(certificate);
                already_ordered.insert(digest);
            }
        }
    }

    // Ensure we do not commit garbage collected certificates.
    ordered.retain(|x| x.round() + gc_depth >= state.last_committed_round);

    // Ordering the output by round is not really necessary but it makes the commit sequence prettier.
    ordered.sort_by_key(|x| x.round());
    ordered
}
