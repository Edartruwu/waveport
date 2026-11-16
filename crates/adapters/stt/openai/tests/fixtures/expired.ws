# A session that was held open, with nothing sent on it, until the
# provider ended it: an hour after it began. The provider says why, and
# two seconds later closes the connection.
# Recorded from the provider on 15 November 2026.
< {"type":"session.created","event_id":"event_EkaJAiuC4NxlIZDPreUZN","session":{"type":"transcription","object":"realtime.transcription_session","id":"sess_EkaJAKAcnJghZq4g6QfJw","expires_at":1794774964,"audio":{"input":{"format":{"type":"audio/pcm","rate":24000},"transcription":null,"noise_reduction":null,"turn_detection":{"type":"server_vad","threshold":0.5,"prefix_padding_ms":300,"silence_duration_ms":200}}},"include":null}}
> "type":"session.update"
< {"type":"session.updated","event_id":"event_EkaJAdJSIIpgHF5GFHCJO","session":{"type":"transcription","object":"realtime.transcription_session","id":"sess_EkaJAKAcnJghZq4g6QfJw","expires_at":1794774964,"audio":{"input":{"format":{"type":"audio/pcm","rate":24000},"transcription":{"model":"gpt-live-transcribe","language":null,"languages":null,"prompt":null},"noise_reduction":null,"turn_detection":null}},"include":null}}
< {"type":"error","event_id":"event_EkbFE8vAVBes8YeVYB7F6","error":{"type":"invalid_request_error","code":"session_expired","message":"Your session hit the maximum duration of 60 minutes.","param":null,"event_id":null}}
! 1001 Your session hit the maximum duration of 60 minutes.
