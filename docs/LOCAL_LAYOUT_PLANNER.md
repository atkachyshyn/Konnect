# LocalLayoutPlanner

`LocalLayoutPlanner` is the deterministic local schematic layout core used by
net materialization, wired-local representation refactors, and the generic
`optimize_schematic_layout` Top-K review operation.

It operates on a bounded local scope of selected nets/items. It does not attempt
whole-sheet placement or global optimality. Konnect owns geometry, hard-contact
rejection, pin escape, obstacle-aware Manhattan routing, deterministic scoring,
and Top-K retention; the LLM supplies electrical/semantic intent and may choose
among returned candidates.

Routing uses A* over `(x, y, incoming_direction)` with Manhattan distance as the
destination heuristic. Search expands through fixed progressive grid bounds and
rejects prohibited geometry rather than assigning it a large finite cost.

The first implementation retains up to three candidates by default and caps
explicit requests at five. Candidate ordering is stable and based on named score
constants plus a structural route signature tie-breaker.
