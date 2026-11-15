# A request the provider refuses for want of audio, and then a question.
# Recorded from the provider on 15 November 2026.
< {"type":"session.created","event_id":"event_EkaEmtIwPSRrkZBbRAE4Y","session":{"type":"transcription","object":"realtime.transcription_session","id":"sess_EkaEmoi1DiXGDrhxccZPT","expires_at":1794774692,"audio":{"input":{"format":{"type":"audio/pcm","rate":24000},"transcription":null,"noise_reduction":null,"turn_detection":{"type":"server_vad","threshold":0.5,"prefix_padding_ms":300,"silence_duration_ms":200}}},"include":null}}
> "type":"session.update"
< {"type":"session.updated","event_id":"event_EkaEn2MvZnTe7m0u4HD9k","session":{"type":"transcription","object":"realtime.transcription_session","id":"sess_EkaEmoi1DiXGDrhxccZPT","expires_at":1794774692,"audio":{"input":{"format":{"type":"audio/pcm","rate":24000},"transcription":{"model":"gpt-live-transcribe","language":null,"languages":null,"prompt":null},"noise_reduction":null,"turn_detection":null}},"include":null}}
> "type":"input_audio_buffer.commit"
< {"type":"error","event_id":"event_EkaEooWPLU9LH6SFRvTFT","error":{"type":"invalid_request_error","code":"input_audio_buffer_commit_empty","message":"Error committing input audio buffer: buffer too small. Expected at least 100ms of audio, but buffer only has 50.00ms of audio.","param":null,"event_id":"commit_1"}}
> "type":"input_audio_buffer.append"
< {"type":"conversation.item.input_audio_transcription.delta","event_id":"event_EkaErUgU5aFVSf9hEarTk","item_id":"item_EkaEq157cQAOxGBKCcrXP","content_index":0,"delta":" What","obfuscation":"vn0auooU8Ql"}
> "type":"input_audio_buffer.append"
< {"type":"conversation.item.input_audio_transcription.delta","event_id":"event_EkaEsgbbhKysE2SCI4IMq","item_id":"item_EkaEq157cQAOxGBKCcrXP","content_index":0,"delta":" time","obfuscation":"swcnZIv1KF9"}
> "type":"input_audio_buffer.append"
< {"type":"conversation.item.input_audio_transcription.delta","event_id":"event_EkaEs6XHliKcyWCVvPQAQ","item_id":"item_EkaEq157cQAOxGBKCcrXP","content_index":0,"delta":" is","obfuscation":"75m3JxgsmkzxL"}
> "type":"input_audio_buffer.append"
< {"type":"conversation.item.input_audio_transcription.delta","event_id":"event_EkaEsNI0ZFH8IJTElPtpb","item_id":"item_EkaEq157cQAOxGBKCcrXP","content_index":0,"delta":" it","obfuscation":"cxWmBlFf0JaUx"}
> "type":"input_audio_buffer.append"
< {"type":"conversation.item.input_audio_transcription.delta","event_id":"event_EkaEsABApMHQl4zPuSE9o","item_id":"item_EkaEq157cQAOxGBKCcrXP","content_index":0,"delta":" right","obfuscation":"D7YAqSYzPx"}
> "type":"input_audio_buffer.commit"
< {"type":"conversation.item.input_audio_transcription.delta","event_id":"event_EkaEspN5OcY0hTXr4NOQY","item_id":"item_EkaEq157cQAOxGBKCcrXP","content_index":0,"delta":" now","obfuscation":"9ec0P5voTsJl"}
< {"type":"conversation.item.input_audio_transcription.delta","event_id":"event_EkaEs0er7vuS001t22KoU","item_id":"item_EkaEq157cQAOxGBKCcrXP","content_index":0,"delta":"?","obfuscation":"PN6nM2PCM0oA3jA"}
< {"type":"input_audio_buffer.committed","event_id":"event_EkaEtrVaFQKSY7Gr92US4","previous_item_id":null,"item_id":"item_EkaEq157cQAOxGBKCcrXP"}
< {"type":"conversation.item.added","event_id":"event_EkaEtDWZ7mTaNiFfRr9Nk","previous_item_id":null,"item":{"id":"item_EkaEq157cQAOxGBKCcrXP","type":"message","status":"completed","role":"user","content":[{"type":"input_audio","transcript":null}]}}
< {"type":"conversation.item.done","event_id":"event_EkaEtEJYaxoqYcZjVT8jN","previous_item_id":null,"item":{"id":"item_EkaEq157cQAOxGBKCcrXP","type":"message","status":"completed","role":"user","content":[{"type":"input_audio","transcript":null}]}}
< {"type":"conversation.item.input_audio_transcription.completed","event_id":"event_EkaEtcc6b64Wta9EQ7IlO","item_id":"item_EkaEq157cQAOxGBKCcrXP","content_index":0,"transcript":"What time is it right now?","usage":{"type":"duration","seconds":3}}
