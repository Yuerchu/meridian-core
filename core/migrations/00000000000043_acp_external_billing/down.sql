-- Restore the classification understood by the previous writer. This is the
-- same narrow class as the up migration; ordinary external rows are untouched.
UPDATE audit_messages
   SET billing_mode = 'metered'
 WHERE billing_mode = 'external'
   AND turn_origin = 'claude_code';
