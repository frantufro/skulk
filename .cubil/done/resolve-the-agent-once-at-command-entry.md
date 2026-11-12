---
created: 2026-11-12
---

# Resolve the Agent once at command entry

Source: architecture review 2026-11-12, candidate 3 (Worth exploring). Depends on the lifecycle task for the existence check.

Problem: AgentRef is a sound value type, and each handler repeats validate_name (12 sites), AgentRef construction (~55 sites), inventory fetch, existence check and classify_agent_error (34 sites). interact.rs alone has 13 AgentRef sites. wait --all round-trips a qualified name through from_qualified and name().

Change: a resolution step turns the user's name into a validated, qualified, existence-checked Agent; error classification moves inside it.

Wins: one interface across ~55 call sites; missing-Agent error text lives in one place.
