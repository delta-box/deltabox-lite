# Commit/abort counts from verified replays (20260217_mini-v2.0.0_claude-4-5-sonnet-high)

500 trajectories replayed in their SWE-bench images; 496 verified end to end. Commit = after any command that changed /testbed (per_write); abort = a command that put the whole /testbed back to an earlier state, replaced by a rollback and verified by replay.

| repository | trajectories | commands | commits (per_write) | aborts | commands per commit | aborts per 100 commits |
|---|---:|---:|---:|---:|---:|---:|
| django/django | 231 | 11344 | 2273 | 30 | 5.0 | 1.3 |
| sympy/sympy | 75 | 4123 | 942 | 24 | 4.4 | 2.5 |
| sphinx-doc/sphinx | 44 | 2945 | 403 | 21 | 7.3 | 5.2 |
| matplotlib/matplotlib | 34 | 1914 | 472 | 2 | 4.1 | 0.4 |
| scikit-learn/scikit-learn | 32 | 1682 | 316 | 2 | 5.3 | 0.6 |
| astropy/astropy | 22 | 1152 | 228 | 1 | 5.1 | 0.4 |
| pydata/xarray | 22 | 1260 | 278 | 12 | 4.5 | 4.3 |
| pytest-dev/pytest | 19 | 1043 | 192 | 9 | 5.4 | 4.7 |
| pylint-dev/pylint | 10 | 703 | 128 | 3 | 5.5 | 2.3 |
| psf/requests | 8 | 394 | 98 | 0 | 4.0 | 0.0 |
| mwaskom/seaborn | 2 | 127 | 29 | 0 | 4.4 | 0.0 |
| pallets/flask | 1 | 38 | 7 | 0 | 5.4 | 0.0 |
| **total** | 500 | 26725 | 5366 | 104 | 5.0 | 1.9 |

* trajectories with at least one abort: 68 of 500
* aborts dropped because replaying with the rollback changed a later step: 7
* exit codes equal to the recording: 26098 of 26224

## Not verified

The verify replay diverged with no abort before the divergence (the environment or a non-deterministic command), so these counts are unverified:

* matplotlib__matplotlib-24870: diverged at step 17
* scikit-learn__scikit-learn-25102: diverged at step 6
* scikit-learn__scikit-learn-25232: diverged at step 11
* scikit-learn__scikit-learn-26323: diverged at step 44
