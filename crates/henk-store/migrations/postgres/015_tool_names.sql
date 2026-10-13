-- A tool name a model invented could be prose thousands of characters long
-- (#341). New calls store it cut to 64 characters, ending in " [cut]";
-- the calls from before are cut the same way.
UPDATE tool_calls SET tool = left(tool, 58) || ' [cut]' WHERE char_length(tool) > 64;
