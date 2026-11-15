// GAP default Cedar policy set (spec/07-security.md section 5, SEC-020..024).
// Posture: default deny; forbid always overrides permit.

@id("permit-agents-readonly")
permit (
    principal in Group::"agents-readonly",
    action in [Action::"Discover", Action::"Execute", Action::"Traverse"],
    resource
) when {
    resource.sideEffect == "NONE"
        && resource.trust != "T2"
        && resource.status == "ACTIVE"
        && principal.tenant == resource.tenant
};

@id("permit-scoped-readers")
permit (
    principal,
    action in [Action::"Discover", Action::"Execute", Action::"Traverse"],
    resource
) when {
    principal.scopes.contains("read:all")
        && resource.sideEffect == "NONE"
        && resource.trust != "T2"
        && principal.tenant == resource.tenant
        && context.riskScore < 80
};

@id("permit-operators-mutate")
permit (
    principal in Group::"operators",
    action in [Action::"Discover", Action::"Execute", Action::"Traverse", Action::"Mutate", Action::"SubmitTask", Action::"CancelTask"],
    resource
) when {
    principal.tenant == resource.tenant
        && resource.trust != "T2"
};

@id("permit-scoped-mutators")
permit (
    principal,
    action in [Action::"Mutate", Action::"SubmitTask", Action::"CancelTask"],
    resource
) when {
    principal.scopes.contains("mutate:run")
        && principal.tenant == resource.tenant
        && resource.trust != "T2"
};

@id("permit-tenant-admins")
permit (
    principal in Group::"admins",
    action in [Action::"Discover", Action::"Execute", Action::"Traverse", Action::"Mutate", Action::"SubmitTask", Action::"DecideApproval", Action::"CancelTask", Action::"ProposeSource", Action::"DecideProposal", Action::"AdminSchema", Action::"AdminIngest", Action::"AdminImpersonate", Action::"AdminPolicy"],
    resource
) when {
    principal.tenant == resource.tenant
        || principal.scopes.contains("admin:impersonate")
};

@id("forbid-cross-tenant-without-impersonate")
forbid (
    principal,
    action,
    resource
) when {
    principal.tenant != resource.tenant
        && !(principal.scopes.contains("admin:impersonate"))
};

@id("forbid-mutate-untrusted-t2")
forbid (
    principal,
    action == Action::"Mutate",
    resource
) when {
    resource.trust == "T2"
};

@id("forbid-mutate-irreversible-or-unknown-unapproved")
forbid (
    principal,
    action == Action::"Mutate",
    resource
) when {
    (resource.sideEffect == "IRREVERSIBLE" || resource.sideEffect == "UNKNOWN")
        && !(context.approved && context.approverIsDifferent)
};

@id("forbid-mutate-rule-of-two-high-risk")
forbid (
    principal,
    action == Action::"Mutate",
    resource
) when {
    context.riskScore >= 100
        && !(context.approved && context.approverIsDifferent)
};
