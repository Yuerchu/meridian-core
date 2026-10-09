-- Where a user message came from. NULL means typed; 'voice' marks offline
-- speech-to-text, which the agent layer uses to stay tolerant of homophone
-- and dropped-word errors in the transcript.
ALTER TABLE messages ADD COLUMN source TEXT;
