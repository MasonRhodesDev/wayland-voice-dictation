use anyhow::Result;

pub trait TranscriptionEngine: Send + Sync {
    fn process_audio(&self, samples: &[i16]) -> Result<()>;

    fn get_current_text(&self) -> Result<String>;

    fn get_final_result(&self) -> Result<String>;

    fn get_cached_text(&self) -> String;

    fn get_audio_buffer(&self) -> Vec<i16>;

    fn reset(&self);
}
