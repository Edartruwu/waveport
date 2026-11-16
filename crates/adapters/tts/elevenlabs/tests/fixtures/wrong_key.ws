# A key the provider does not know. The connection is made all the same, and the provider says so
# once a speech has been asked for on it.
# Recorded from the provider on 16 November 2026. The `<` line is the provider's message as it came,
# and each `>` names a message it was sent before: a context begun, "Hello." to speak, and its end.
> "text":" "
> "text":"Hello."
> "close_context":true
< {"message":"Invalid API key","error":"invalid_api_key","code":1008}
