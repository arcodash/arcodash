/** Tag for a mutation whose own 401 means the credentials just submitted were wrong (e.g. a
 * current-password check), not that the session expired — split into its own module so the
 * `MutationCache` in `sessionExpiry.ts` and the pages that need the exemption can both depend on it
 * without importing each other. */
export const CREDENTIAL_CHECK_MUTATION_KEY = "credential-check";
