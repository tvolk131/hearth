# Disposable Proxmox quota workflow qualification

The final V3 Rust quota workflow **passed** against actual Proxmox VE 9.2.26 and a real ZFS mirror inside a disposable VM. It used the explicit `physical_trial` backend, all three frozen portable Linux binaries, an unprivileged controller, a root helper and real host-local iroh connections from the Linux CLI over pinned lab SSH. No physical Hetzner host was contacted.

The authoritative result is the complete mode report. All five required behavior flags, immutable-scope reopen, exact unmanaged-resource preservation and `owned_qemu_stopped` are true. The run took 3773.5 seconds under TCG. It used a blank guest disk and does not claim nested guest OS boot or cross-host iroh qualification.

A later supervised two-host hardware trial completed real quota growth and unused contraction on both hosts, source below-commitment refusal, a bootable native stopped copy and explicit VM/account cleanup. Existing guests and allocations were preserved. This used host-local iroh through pinned SSH; it does not qualify public iroh traversal. A narrow cloud-init metadata repair was required, so automatic trial reruns remain disabled.

## Observed workflow

The fresh 16 GiB mirror held a pinned 12 GiB trial parent. Before creating a 6 GiB member allocation, globally unreserved pool space was only 3,237,216,256 bytes while parent `available` was 12,750,487,552 bytes. Successful admission therefore exercised the prepaid-parent accounting path rather than requiring the same reservation again from globally unreserved pool space.

| Verified boundary | Owner quota | Owner reservation | Catalog result |
| --- | ---: | ---: | --- |
| Initial empty member | 6144 MiB | 6144 MiB | Ready, revision 0 |
| Real growth effect before acknowledgement | 7168 MiB | 6144 MiB | Old policy retained, frozen at revision 0 |
| Same-boot helper recovery | 7168 MiB | 7168 MiB | Full target policy committed, revision 1 |
| Safe unused contraction with a stopped VM | 6144 MiB | 6144 MiB | VM configuration preserved, revision 2 |
| CPU-only policy edit | 6144 MiB | 6144 MiB | Storage contract preserved, revision 3 |
| Explicit VM deletion, then empty-account deletion | Absent | Absent | Verified tombstones, devices revoked, commitments released |

Growth also raised memory, core and VM-count admission limits. Contraction restored limits that still covered the configured VM. The blank VM had a 128 MiB thick root disk, cloud-init backing and a modeled 140 MiB storage commitment. A valid 64 MiB target below that commitment was refused; guest filesystem utilization did not affect the decision.

The run also refused zero, stale revision, changed-payload retry, member policy editing, foreign VM actions by both member and administrator, and an owner outside the trial allowlist. Refusals added no operations and changed neither policy nor storage. An exact accepted retry retained its operation ID. Replaying the earlier successful growth after contraction returned historical proof without restoring its old quota. The CPU-only edit changed its policy revision while preserving the storage contract.

Before and after contraction, refusals, historical retry and the CPU-only edit, the fixture compared exact owner GUID/creation/marker, descendant inventory, root/cloud-init GUIDs and thick reservations, full PVE VM configurations and storage binding. It then verified start, stop and nonpurging VM deletion before retiring the empty account through the Rust API. Account retirement exercised the real `ro,zfsutil` empty-filesystem inspection, exact storage/dataset absence, tombstones and member revocation.

## Real effect interruption

Only inside the fresh disposable root overlay, a nonce/config/machine-bound wrapper ran the original ZFS executable and stopped itself after the exact successful `zfs set quota=7516192768 <trial-owner>` effect. A marker and independent property read proved the real quota effect was present while its response was withheld from the unchanged helper binary.

The fixture captured the 7 GiB/6 GiB property pair and old frozen catalog policy, killed the helper service's own process group, restored the original ZFS executable byte-for-byte, and restarted the helper. It verified a changed helper PID in the same kernel boot, the same operation ID, final 7 GiB/7 GiB properties, full target policy revision and unchanged dataset/storage identities. See the recovery receipt and adjacent effect/frozen/recovered snapshots.

This qualifies the selected same-boot case with an independently observed effect present. It does not establish recovery from every real crash boundary, an absent ambiguous same-boot effect, physical power loss, or racing external administration. Generic blocked-operation reconciliation remains a separate requirement.

## Final preservation and artifact pins

After stopping controller admission, the fixture inspected the offline catalog and helper provenance: every operation was verified, every managed VM was deleted, and no helper job remained unsettled. The final PVE inventory contained only the original stopped/protected unmanaged VM 103. Its full configuration, the unrelated dataset's immutable properties, and the bytes of global jobs/user/HA/firewall configuration matched their baselines exactly. Only the trial root, control and administrator filesystems remained; no managed VM or orphan zvol remained.

Root-marker drift and a changed immutable GUID were refused; restoring the original scope reopened schema-4 services successfully. The original lab ID and pool-wide lab marker were unchanged. The runtime hashes of all three executed portable binaries matched the frozen V3 bundle, whose source-manifest SHA-256 is `b9bdcca5eb5a45429ec0b67c8bdaddf633f06e3080e4f19e8192671a9972ce7f`. See the portable build manifest and final inventory.

Reproduction from `pve-dev/`, with the matching sealed cached base and a frozen bundle containing its checked build manifest:

```sh
nix develop ./nix --command python3 lab/physical_trial_suite.py \
  --linux-bin-dir .lab/physical-trial-binaries-20261111-v3 --timeout 1800
```

The standalone command holds the shared lab lock. The observed run used the reviewed fixed-task coordinator and its inherited lock; it did not run an independent competing emulator. The fixture's six focused tests and the older quota harness's eleven focused tests passed; all 167 integrated Rust tests, formatting and Clippy with warnings denied also passed before the exact V3 artifact freeze.

## Retained earlier partial run

The earlier generic-lab frozen build remains **incomplete**, with its partial report unchanged. It verified growth, below-commitment/stale/zero refusal, contraction and a CPU-only edit, but its fixed 300-second recovery marker deadline expired before a conclusive effect/acknowledgement boundary. Remaining lifecycle/final inventory checks did not run. QEMU was stopped/reaped and its lab lock was released.

Offline inspection of only the two known journal files from its inactive qcow2 image found prior policy operations verified, but the interrupted growth's first property intent unverified. The catalog retained 96 MiB, revision 3, `ready=false` and conservative claims. The exact property-effect versus marker timing was not captured, so that run supplies no recovery success claim. Its source-manifest SHA-256 is `3ffb05677b0f7be6beb55437839a9c9ab61b55b35732f30b249ee79c309d6b80`. The passing current V3 report above supplies the separately observed effect-present recovery proof with the full operation deadline.
