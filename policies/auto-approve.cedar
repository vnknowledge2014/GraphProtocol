// GAP optional low-risk reversible auto-approve policy (RUN-071, SEC-022).
// Reversible T0 operations at low session risk are explicitly permitted without a prior
// human approval task, while IRREVERSIBLE/UNKNOWN mutations still require four-eyes approval.

@id("permit-auto-approve-t0-reversible")
permit (
    principal in Group::"operators",
    action == Action::"Mutate",
    resource
) when {
    principal.tenant == resource.tenant
        && resource.trust == "T0"
        && resource.sideEffect == "REVERSIBLE"
        && context.riskScore < 30
};
