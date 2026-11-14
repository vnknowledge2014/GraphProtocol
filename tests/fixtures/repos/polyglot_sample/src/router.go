package router

import "fmt"

// RequestRouter routes incoming RPC frames after credential validation.
type RequestRouter struct {
	ServiceName string
}

// NewRouter constructs a router configured for a named backend service.
func NewRouter(service string) RequestRouter {
	return RequestRouter{ServiceName: service}
}

// Dispatch authorizes the inbound credential and logs routing metadata.
func (r *RequestRouter) Dispatch(route string, token string) string {
	if !validate_token(token) {
		return fmt.Sprintf("rejected:%s:%s", r.ServiceName, route)
	}
	return fmt.Sprintf("accepted:%s:%s", r.ServiceName, route)
}
