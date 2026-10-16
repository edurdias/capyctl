# Status: Launcher tests wait for reparented zombies to be reaped — 2026-10-16 (branch `test/launchers-orphan-zombie`)

`owned_launch::tests::an_unrecorded_worker_of_a_dead_held_leader_is_terminated` failed once in
hosted CI (PR #130): right after `terminate_escalating` returned, `observe_group` still listed the
terminated worker. The worker had exited (`runs` read false), but its leader died first, so it
was reparented to init and stayed a zombie in the process group until init reaped it, which on a
loaded runner is not immediate. Four tests asserted an empty group at the very next scan; they
now go through `assert_group_empties`, which allows up to 5 s for the group to empty. The
product path is unchanged: Terminate already treats an exited process as gone. Test only;
`cargo test -p capyctl-launchers --lib owned_launch` passed 6 of 6 times under 4 cores.

# Release note: none
