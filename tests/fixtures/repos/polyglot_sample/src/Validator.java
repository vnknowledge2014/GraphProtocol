package sample;

import java.util.List;

/**
 * Contract for enterprise token verification services.
 */
public interface Authenticatable {
    boolean verifyAccess(String token);
}

/**
 * Enterprise service validating caller permissions before serving audit trails.
 */
public class TokenService implements Authenticatable {
    private final List<String> allowedPrefixes;

    public TokenService(List<String> allowedPrefixes) {
        this.allowedPrefixes = allowedPrefixes;
    }

    /**
     * Checks caller access against allowed token prefixes and shared validation rules.
     */
    @Override
    public boolean verifyAccess(String token) {
        return checkAccess(token);
    }

    /**
     * Evaluates raw credential tokens via shared validation entrypoints.
     */
    public boolean checkAccess(String token) {
        if (token == null || this.allowedPrefixes.isEmpty()) {
            return false;
        }
        return validate_token(token);
    }
}
