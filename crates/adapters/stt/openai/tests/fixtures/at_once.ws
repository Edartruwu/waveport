# A question sent all at once, so that the request arrives before anything is written.
# Recorded from the provider on 15 November 2026.
< {"type":"session.created","event_id":"event_Eka2bAH6CRglOeNoka0z3","session":{"type":"transcription","object":"realtime.transcription_session","id":"sess_Eka2bbMg15AEPp0mko7x1","expires_at":1794773937,"audio":{"input":{"format":{"type":"audio/pcm","rate":24000},"transcription":null,"noise_reduction":null,"turn_detection":{"type":"server_vad","threshold":0.5,"prefix_padding_ms":300,"silence_duration_ms":200}}},"include":null}}
> "type":"session.update"
< {"type":"session.updated","event_id":"event_Eka2b4PTckIsyVEEmQhgh","session":{"type":"transcription","object":"realtime.transcription_session","id":"sess_Eka2bbMg15AEPp0mko7x1","expires_at":1794773937,"audio":{"input":{"format":{"type":"audio/pcm","rate":24000},"transcription":{"model":"gpt-live-transcribe","language":null,"languages":null,"prompt":null},"noise_reduction":null,"turn_detection":null}},"include":null}}
> "type":"input_audio_buffer.commit"
< {"type":"input_audio_buffer.committed","event_id":"event_Eka2cj3IV9BEmfrrE6wEk","previous_item_id":null,"item_id":"item_Eka2c43xHdHODnKxLEJbb"}
< {"type":"conversation.item.added","event_id":"event_Eka2cLGkftvURzJgtxfQb","previous_item_id":null,"item":{"id":"item_Eka2c43xHdHODnKxLEJbb","type":"message","status":"completed","role":"user","content":[{"type":"input_audio","transcript":null}]}}
< {"type":"conversation.item.done","event_id":"event_Eka2cjIHQtrMiquRV0XB5","previous_item_id":null,"item":{"id":"item_Eka2c43xHdHODnKxLEJbb","type":"message","status":"completed","role":"user","content":[{"type":"input_audio","transcript":null}]}}
< {"type":"conversation.item.input_audio_transcription.delta","event_id":"event_Eka2dOjlWLBwJMBx6MWUk","item_id":"item_Eka2c43xHdHODnKxLEJbb","content_index":0,"delta":" What","obfuscation":"neMo7bJvo2Q"}
< {"type":"conversation.item.input_audio_transcription.delta","event_id":"event_Eka2ddc2Rq0gArP2ryjZd","item_id":"item_Eka2c43xHdHODnKxLEJbb","content_index":0,"delta":" time","obfuscation":"0JTSDKRIjfS"}
< {"type":"conversation.item.input_audio_transcription.delta","event_id":"event_Eka2d0YrnewFKZUJc2zUa","item_id":"item_Eka2c43xHdHODnKxLEJbb","content_index":0,"delta":" is","obfuscation":"0lOezFCmn86e3"}
< {"type":"conversation.item.input_audio_transcription.delta","event_id":"event_Eka2dkkRI9jkwwL6OURwV","item_id":"item_Eka2c43xHdHODnKxLEJbb","content_index":0,"delta":" it","obfuscation":"AjMUODAQHM8ZR"}
< {"type":"conversation.item.input_audio_transcription.delta","event_id":"event_Eka2dZWpokXQn86HxXRa4","item_id":"item_Eka2c43xHdHODnKxLEJbb","content_index":0,"delta":" right","obfuscation":"IzffT0qAom"}
< {"type":"conversation.item.input_audio_transcription.delta","event_id":"event_Eka2dnsQ6CKnedcgsoUA6","item_id":"item_Eka2c43xHdHODnKxLEJbb","content_index":0,"delta":" now","obfuscation":"sKmZ6vf4Nwep"}
< {"type":"conversation.item.input_audio_transcription.delta","event_id":"event_Eka2dKuPiYTG7EYH7Qllw","item_id":"item_Eka2c43xHdHODnKxLEJbb","content_index":0,"delta":"?","obfuscation":"reIZfDy5Tdh4lio"}
< {"type":"conversation.item.input_audio_transcription.completed","event_id":"event_Eka2d0vMjesJnV28yhVWa","item_id":"item_Eka2c43xHdHODnKxLEJbb","content_index":0,"transcript":"What time is it right now?","usage":{"type":"duration","seconds":3}}
