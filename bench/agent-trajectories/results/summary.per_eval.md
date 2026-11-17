# Replay results (per_eval policy)

## Fidelity

`rc = recorded`: commands whose exit code matches the recording. `tree = manual`: transaction boundaries where the workspace hash equals the `manual` run's (the agent's own commands).

| instance | backend | steps | rc = recorded | tree = manual | aborts |
|---|---|---:|---:|---:|---:|
| django__django-11433 | manual | 57 | 56/56 | 27/27 | 1 |
| django__django-11433 | git | 57 | 56/56 | 27/27 | 1 |
| sympy__sympy-13372 | manual | 43 | 42/42 | 14/14 | 3 |
| sympy__sympy-13372 | git | 43 | 42/42 | 14/14 | 3 |
| sympy__sympy-13877 | manual | 66 | 65/65 | 25/25 | 0 |
| sympy__sympy-13877 | git | 66 | 65/65 | 25/25 | 0 |
| scikit-learn__scikit-learn-11310 | manual | 66 | 65/65 | 17/17 | 1 |
| scikit-learn__scikit-learn-11310 | git | 66 | 65/65 | 17/17 | 1 |
| sphinx-doc__sphinx-8595 | manual | 68 | 67/67 | 19/19 | 1 |
| sphinx-doc__sphinx-8595 | git | 68 | 67/67 | 19/19 | 1 |
| pylint-dev__pylint-4551 | manual | 105 | 104/104 | 49/49 | 0 |
| pylint-dev__pylint-4551 | git | 105 | 104/104 | 49/49 | 0 |
| django__django-14373 | manual | 27 | 26/26 | 11/11 | 0 |
| django__django-14373 | git | 27 | 26/26 | 11/11 | 0 |
| django__django-12308 | manual | 50 | 49/49 | 19/19 | 0 |
| django__django-12308 | git | 50 | 49/49 | 19/19 | 0 |

## Cost

Milliseconds. Agent commands are the trajectory's own steps (identical work in every backend); commit/abort are the backend's operations. For `manual`, an abort is the agent's own revert command and a commit costs nothing.

| backend | agent commands (total s) | commits | commit median | commit total (s) | aborts | abort median | abort total (s) | overhead / command time |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| manual | 166.4 | 175 | 0.0 | 0.00 | 6 | 4.8 | 0.04 | 0.0% |
| git | 170.7 | 175 | 19.7 | 4.40 | 6 | 12.1 | 0.13 | 2.7% |
