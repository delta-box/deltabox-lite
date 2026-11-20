# Replay results (per_write policy)

## Fidelity

`rc = recorded`: commands whose exit code matches the recording. `tree = manual`: transaction boundaries where the workspace hash equals the `manual` run's (the agent's own commands).

| instance | backend | steps | rc = recorded | tree = manual | aborts |
|---|---|---:|---:|---:|---:|
| django__django-11433 | manual | 57 | 56/56 | 20/20 | 1 |
| django__django-11433 | git | 57 | 56/56 | 20/20 | 1 |
| sympy__sympy-13372 | manual | 43 | 42/42 | 9/9 | 3 |
| sympy__sympy-13372 | git | 43 | 42/42 | 9/9 | 3 |
| sympy__sympy-13877 | manual | 66 | 65/65 | 18/18 | 0 |
| sympy__sympy-13877 | git | 66 | 65/65 | 18/18 | 0 |
| sphinx-doc__sphinx-8595 | manual | 68 | 67/67 | 4/4 | 1 |
| sphinx-doc__sphinx-8595 | git | 68 | 67/67 | 4/4 | 1 |
| pylint-dev__pylint-4551 | manual | 105 | 104/104 | 28/28 | 0 |
| pylint-dev__pylint-4551 | git | 105 | 104/104 | 28/28 | 0 |
| django__django-14373 | manual | 27 | 26/26 | 5/5 | 0 |
| django__django-14373 | git | 27 | 26/26 | 5/5 | 0 |
| django__django-12308 | manual | 50 | 49/49 | 11/11 | 0 |
| django__django-12308 | git | 50 | 49/49 | 11/11 | 0 |

## Cost

Milliseconds. Agent commands are the trajectory's own steps (identical work in every backend); commit/abort are the backend's operations. For `manual`, an abort is the agent's own revert command and a commit costs nothing.

| backend | agent commands (total s) | commits | commit median | commit total (s) | aborts | abort median | abort total (s) | overhead / command time |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| manual | 133.5 | 90 | 0.0 | 0.00 | 5 | 5.4 | 0.03 | 0.0% |
| git | 131.9 | 90 | 19.7 | 2.45 | 5 | 12.2 | 0.13 | 2.0% |
