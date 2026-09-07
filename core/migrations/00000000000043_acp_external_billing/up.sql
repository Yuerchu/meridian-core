-- Live Claude Code turns have always been paid outside Meridian. Migration 41
-- introduced the `external` value but the shared transcript writer still used
-- its default `metered`, so those rows appeared as a price the user had failed
-- to configure. Match the request-level origin and the exact hosted-provider
-- origin written by that path; no conversation or provider join is used because
-- audit rows deliberately outlive both, and provider_name is display text rather
-- than billing identity.
UPDATE audit_messages
   SET billing_mode = 'external'
 WHERE billing_mode = 'metered'
   AND turn_origin = 'claude_code';
