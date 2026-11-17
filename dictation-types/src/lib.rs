#[derive(Debug, Clone)]
pub enum GuiControl {
    Initialize,

    SetHidden,

    SetListening,

    UpdateTranscription { text: String, is_final: bool },

    UpdateSpectrum(Vec<f32>),

    UpdateVadState { is_speaking: bool, text_settled: bool },

    SetTranscribing,

    SetTyping { done: usize, total: usize },

    SetClosing,

    Exit,
}

#[derive(Debug, Clone)]
pub enum GuiStatus {
    Ready,

    TransitionComplete { from: GuiState, to: GuiState },

    Error(String),

    ShuttingDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuiState {
    Hidden,
    PreListening,
    Listening,
    Transcribing,
    Typing,
    Closing,
}
