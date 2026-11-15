# The same question twice on one connection, each with its request.
# Recorded from the provider on 15 November 2026.
< {"type":"session.created","event_id":"event_Eka2LhvfTNteQgKyaclPZ","session":{"type":"transcription","object":"realtime.transcription_session","id":"sess_Eka2L1ZXI0kLRqjKJYhQM","expires_at":1794773921,"audio":{"input":{"format":{"type":"audio/pcm","rate":24000},"transcription":null,"noise_reduction":null,"turn_detection":{"type":"server_vad","threshold":0.5,"prefix_padding_ms":300,"silence_duration_ms":200}}},"include":null}}
> "type":"session.update"
< {"type":"session.updated","event_id":"event_Eka2MlZXmrTIV5ydH3M9W","session":{"type":"transcription","object":"realtime.transcription_session","id":"sess_Eka2L1ZXI0kLRqjKJYhQM","expires_at":1794773921,"audio":{"input":{"format":{"type":"audio/pcm","rate":24000},"transcription":{"model":"gpt-live-transcribe","language":null,"languages":null,"prompt":null},"noise_reduction":null,"turn_detection":null}},"include":null}}
> "type":"input_audio_buffer.append"
< {"type":"conversation.item.input_audio_transcription.delta","event_id":"event_Eka2Or0Q7zrNYpibQhoKo","item_id":"item_Eka2N7GXN83TwtLZjGt6Y","content_index":0,"delta":" What","obfuscation":"8ooCIbIzPcX"}
> "type":"input_audio_buffer.append"
< {"type":"conversation.item.input_audio_transcription.delta","event_id":"event_Eka2O9ZYcfnMIuCwMCcrn","item_id":"item_Eka2N7GXN83TwtLZjGt6Y","content_index":0,"delta":" time","obfuscation":"9KcBU2pZ4IO"}
> "type":"input_audio_buffer.append"
< {"type":"conversation.item.input_audio_transcription.delta","event_id":"event_Eka2On6c3ZvSGvkUhnYLR","item_id":"item_Eka2N7GXN83TwtLZjGt6Y","content_index":0,"delta":" is","obfuscation":"WpTm8WMEnnH2c"}
> "type":"input_audio_buffer.append"
< {"type":"conversation.item.input_audio_transcription.delta","event_id":"event_Eka2PnrlaLt7WIxdTbRpk","item_id":"item_Eka2N7GXN83TwtLZjGt6Y","content_index":0,"delta":" it","obfuscation":"DQsb6WaJ04w4x"}
> "type":"input_audio_buffer.append"
< {"type":"conversation.item.input_audio_transcription.delta","event_id":"event_Eka2PbKHgwPSycOs14yIE","item_id":"item_Eka2N7GXN83TwtLZjGt6Y","content_index":0,"delta":" right","obfuscation":"gnVRwJyAaE"}
> "type":"input_audio_buffer.commit"
< {"type":"conversation.item.input_audio_transcription.delta","event_id":"event_Eka2PhNAWboPAENWDukhy","item_id":"item_Eka2N7GXN83TwtLZjGt6Y","content_index":0,"delta":" now","obfuscation":"mjfjmVtRqNkC"}
< {"type":"conversation.item.input_audio_transcription.delta","event_id":"event_Eka2P2fvCHCPwjMvt6aOw","item_id":"item_Eka2N7GXN83TwtLZjGt6Y","content_index":0,"delta":"?","obfuscation":"1QZtyEEjme7av1w"}
< {"type":"input_audio_buffer.committed","event_id":"event_Eka2PDsfJYJTPZaDk6JuV","previous_item_id":null,"item_id":"item_Eka2N7GXN83TwtLZjGt6Y"}
< {"type":"conversation.item.added","event_id":"event_Eka2PGU3XjDGPeCDDpQl1","previous_item_id":null,"item":{"id":"item_Eka2N7GXN83TwtLZjGt6Y","type":"message","status":"completed","role":"user","content":[{"type":"input_audio","transcript":null}]}}
< {"type":"conversation.item.done","event_id":"event_Eka2PhShfAJcOombpFinY","previous_item_id":null,"item":{"id":"item_Eka2N7GXN83TwtLZjGt6Y","type":"message","status":"completed","role":"user","content":[{"type":"input_audio","transcript":null}]}}
< {"type":"conversation.item.input_audio_transcription.completed","event_id":"event_Eka2QnF8xtYfiSmeoYPVl","item_id":"item_Eka2N7GXN83TwtLZjGt6Y","content_index":0,"transcript":"What time is it right now?","usage":{"type":"duration","seconds":3}}
> "type":"input_audio_buffer.append"
< {"type":"conversation.item.input_audio_transcription.delta","event_id":"event_Eka2RycA2k7iNOt6Cj3ja","item_id":"item_Eka2PMy5usUoCp1YREfqH","content_index":0,"delta":" What","obfuscation":"N1GFX7jsRVI"}
> "type":"input_audio_buffer.append"
< {"type":"conversation.item.input_audio_transcription.delta","event_id":"event_Eka2SB86301hEQyH4teOm","item_id":"item_Eka2PMy5usUoCp1YREfqH","content_index":0,"delta":" time","obfuscation":"q1UxW5I9kQ6"}
> "type":"input_audio_buffer.append"
< {"type":"conversation.item.input_audio_transcription.delta","event_id":"event_Eka2SuOFVP1cB8WpIqrSw","item_id":"item_Eka2PMy5usUoCp1YREfqH","content_index":0,"delta":" is","obfuscation":"fG6VznvDD61gK"}
> "type":"input_audio_buffer.append"
< {"type":"conversation.item.input_audio_transcription.delta","event_id":"event_Eka2SUao22Gs5rMBSNugl","item_id":"item_Eka2PMy5usUoCp1YREfqH","content_index":0,"delta":" it","obfuscation":"1VIDAHIyIoPSs"}
> "type":"input_audio_buffer.commit"
< {"type":"input_audio_buffer.committed","event_id":"event_Eka2SJzvu4YvJlqUOJ5px","previous_item_id":"item_Eka2N7GXN83TwtLZjGt6Y","item_id":"item_Eka2PMy5usUoCp1YREfqH"}
< {"type":"conversation.item.added","event_id":"event_Eka2S3UUQj3HLC2cAo1WH","previous_item_id":"item_Eka2N7GXN83TwtLZjGt6Y","item":{"id":"item_Eka2PMy5usUoCp1YREfqH","type":"message","status":"completed","role":"user","content":[{"type":"input_audio","transcript":null}]}}
< {"type":"conversation.item.done","event_id":"event_Eka2SQQJVO74ykIDXKefx","previous_item_id":"item_Eka2N7GXN83TwtLZjGt6Y","item":{"id":"item_Eka2PMy5usUoCp1YREfqH","type":"message","status":"completed","role":"user","content":[{"type":"input_audio","transcript":null}]}}
< {"type":"conversation.item.input_audio_transcription.delta","event_id":"event_Eka2SCkxr2mBIKJRbqTUy","item_id":"item_Eka2PMy5usUoCp1YREfqH","content_index":0,"delta":" right","obfuscation":"iCM1ZBL66K"}
< {"type":"conversation.item.input_audio_transcription.delta","event_id":"event_Eka2SxW4ldIbcM1hi4ZKu","item_id":"item_Eka2PMy5usUoCp1YREfqH","content_index":0,"delta":" now","obfuscation":"oPsXQHIStdZe"}
< {"type":"conversation.item.input_audio_transcription.delta","event_id":"event_Eka2SkAKmuFQmJ4yQfv9d","item_id":"item_Eka2PMy5usUoCp1YREfqH","content_index":0,"delta":"?","obfuscation":"xqLgXQqiFL8EbVF"}
< {"type":"conversation.item.input_audio_transcription.completed","event_id":"event_Eka2TQRVXUbYjuhPn7iyw","item_id":"item_Eka2PMy5usUoCp1YREfqH","content_index":0,"transcript":"What time is it right now?","usage":{"type":"duration","seconds":3}}
