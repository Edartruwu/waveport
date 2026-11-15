# A session asked for in a language the provider does not know.
# Recorded from the provider on 15 November 2026.
< {"type":"session.created","event_id":"event_EkaEa3eHnLxYYibh2q3m3","session":{"type":"transcription","object":"realtime.transcription_session","id":"sess_EkaEacnDhWgwJXFxGR8oD","expires_at":1794774680,"audio":{"input":{"format":{"type":"audio/pcm","rate":24000},"transcription":null,"noise_reduction":null,"turn_detection":{"type":"server_vad","threshold":0.5,"prefix_padding_ms":300,"silence_duration_ms":200}}},"include":null}}
> "type":"session.update"
< {"type":"error","event_id":"event_EkaEapcUtGGrd2sRIMiXQ","error":{"type":"invalid_request_error","code":"invalid_value","message":"Invalid value: 'klingon'. Supported values are: 'af', 'ar', 'az', 'be', 'bg', 'bs', 'ca', 'cs', 'cy', 'da', 'de', 'el', 'en', 'es', 'et', 'fa', 'fi', 'fr', 'gl', 'he', 'hi', 'hr', 'hu', 'hy', 'id', 'is', 'it', 'iw', 'ja', 'kk', 'kn', 'ko', 'lt', 'lv', 'mi', 'mk', 'mr', 'ms', 'ne', 'nl', 'no', 'pl', 'pt', 'ro', 'ru', 'sk', 'sl', 'sr', 'sv', 'sw', 'ta', 'th', 'tl', 'tr', 'uk', 'ur', 'vi', and 'zh'.","param":"session.audio.input.transcription.languages[0]","event_id":null}}
> "type":"input_audio_buffer.append"
< {"type":"input_audio_buffer.speech_started","event_id":"event_EkaEcQotO8s0cWzULomlG","audio_start_ms":180,"item_id":"item_EkaEczNaphjxzco7F1XLH"}
> "type":"input_audio_buffer.append"
< {"type":"input_audio_buffer.speech_stopped","event_id":"event_EkaEdb5Gs9KANkyyvETlW","audio_end_ms":2144,"item_id":"item_EkaEczNaphjxzco7F1XLH"}
< {"type":"input_audio_buffer.committed","event_id":"event_EkaEduun6Ok6YqYpnxT9v","previous_item_id":null,"item_id":"item_EkaEczNaphjxzco7F1XLH"}
> "type":"input_audio_buffer.append"
< {"type":"conversation.item.added","event_id":"event_EkaEdy4GGEuaf7CKGOe5s","previous_item_id":null,"item":{"id":"item_EkaEczNaphjxzco7F1XLH","type":"message","status":"completed","role":"user","content":[{"type":"input_audio","transcript":null}]}}
< {"type":"conversation.item.done","event_id":"event_EkaEdKMJhn0AzX1iGSooJ","previous_item_id":null,"item":{"id":"item_EkaEczNaphjxzco7F1XLH","type":"message","status":"completed","role":"user","content":[{"type":"input_audio","transcript":null}]}}
> "type":"input_audio_buffer.commit"
< {"type":"error","event_id":"event_EkaEedRCz1okv8rmtSHxg","error":{"type":"invalid_request_error","code":"input_audio_buffer_commit_empty","message":"Error committing input audio buffer: buffer too small. Expected at least 100ms of audio, but buffer only has 0.00ms of audio.","param":null,"event_id":null}}
