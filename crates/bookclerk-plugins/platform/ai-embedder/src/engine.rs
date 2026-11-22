//! Running a cut graph with ONNX Runtime.
//!
//! ONNX Runtime is loaded once per process from the shared library beside
//! the plugin's executable ([`runtime`]). A [`Graph`] is one model's
//! session; [`Graph::run`] takes the table rows of one text's tokens and
//! gives the graph's output numbers.

use std::path::Path;
use std::sync::{Mutex, OnceLock, PoisonError};

use bookclerk_model_pack::{Element, GraphSpec, Pooling, TableSpec};
use ort::environment::Environment;
use ort::session::{Session, SessionInputValue};
use ort::value::Tensor;

/// Why the runtime could not be loaded or a graph not run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    /// The ONNX Runtime shared library could not be loaded.
    #[error("ONNX Runtime could not be loaded: {0}")]
    Runtime(String),
    /// The graph file is not a graph ONNX Runtime accepts.
    #[error("the model graph could not be loaded: {0}")]
    Graph(String),
    /// The graph lacks an input or output the model description names.
    #[error("the model graph has no {kind} named `{name}`")]
    MissingName {
        /// `input` or `output`.
        kind: &'static str,
        /// The name the description gives.
        name: String,
    },
    /// The rows passed do not fit the tables.
    #[error("the rows passed do not fit the model's tables")]
    Rows,
    /// ONNX Runtime failed while running the graph.
    #[error("the model run failed: {0}")]
    Run(String),
    /// The output is not the shape the model description promises.
    #[error("the model gave {found} numbers where {expected} were expected")]
    OutputShape {
        /// Numbers the description promises for this text.
        expected: usize,
        /// Numbers the graph gave.
        found: usize,
    },
}

/// The process's ONNX Runtime environment, or why loading it failed. Set
/// by the first [`runtime`] call; ONNX Runtime allows one per process.
static RUNTIME: OnceLock<Result<Environment, EngineError>> = OnceLock::new();

/// Loads ONNX Runtime from `library`, once per process.
///
/// # Arguments
///
/// * `library` - Path of the shared library. Later calls return the
///   environment the first call made, whatever path they pass.
///
/// # Errors
///
/// [`EngineError::Runtime`] when the library is missing or is not an ONNX
/// Runtime build the bindings accept; the failure is remembered.
pub fn runtime(library: &Path) -> Result<&'static Environment, EngineError> {
    RUNTIME
        .get_or_init(|| {
            let builder =
                ort::init_from(library).map_err(|err| EngineError::Runtime(err.to_string()))?;
            builder
                .with_name("bookclerk-embedder")
                .with_telemetry(false)
                .build()
                .map_err(|err| EngineError::Runtime(err.to_string()))
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// One model's graph, ready to run.
pub struct Graph {
    /// The ONNX Runtime session; a run needs it exclusively.
    session: Mutex<Session>,
    /// Name of the token-position input.
    ids_input: String,
    /// Name of the attention-mask input.
    mask_input: String,
    /// Name of the output read.
    output: String,
    /// How the output becomes one vector.
    pooling: Pooling,
    /// The tables, in row order.
    tables: Vec<TableSpec>,
    /// Numbers per vector.
    dimensions: usize,
}

impl std::fmt::Debug for Graph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Graph")
            .field("output", &self.output)
            .field("dimensions", &self.dimensions)
            .finish_non_exhaustive()
    }
}

impl Graph {
    /// Loads a graph.
    ///
    /// # Arguments
    ///
    /// * `environment` - The process's ONNX Runtime ([`runtime`]).
    /// * `bytes` - The graph file, already checked against the model
    ///   description.
    /// * `spec` - The graph part of the model description.
    /// * `tables` - The model's tables.
    /// * `dimensions` - Numbers per vector.
    ///
    /// # Errors
    ///
    /// [`EngineError::Graph`] when ONNX Runtime refuses the file, and
    /// [`EngineError::MissingName`] when it lacks a named input or output.
    pub fn load(
        environment: &Environment,
        bytes: &[u8],
        spec: &GraphSpec,
        tables: &[TableSpec],
        dimensions: u32,
    ) -> Result<Self, EngineError> {
        let graph = |err: ort::Error| EngineError::Graph(err.to_string());
        // One thread: a text is tens of milliseconds, and the plugin shares
        // its machine with playback and downloads.
        let session = Session::builder(environment)
            .map_err(graph)?
            .with_intra_threads(1)
            .map_err(|err| EngineError::Graph(err.to_string()))?
            .with_inter_threads(1)
            .map_err(|err| EngineError::Graph(err.to_string()))?
            .commit_from_memory(bytes)
            .map_err(graph)?;
        let inputs = [&spec.ids_input, &spec.mask_input]
            .into_iter()
            .chain(tables.iter().map(|table| &table.input));
        for name in inputs {
            if !session.inputs().iter().any(|input| input.name() == name) {
                return Err(EngineError::MissingName {
                    kind: "input",
                    name: name.clone(),
                });
            }
        }
        if !session
            .outputs()
            .iter()
            .any(|output| output.name() == spec.output)
        {
            return Err(EngineError::MissingName {
                kind: "output",
                name: spec.output.clone(),
            });
        }
        Ok(Self {
            session: Mutex::new(session),
            ids_input: spec.ids_input.clone(),
            mask_input: spec.mask_input.clone(),
            output: spec.output.clone(),
            pooling: spec.pooling,
            tables: tables.to_vec(),
            dimensions: usize::try_from(dimensions).unwrap_or(usize::MAX),
        })
    }

    /// Runs the graph on one text.
    ///
    /// # Arguments
    ///
    /// * `rows` - One row-pack row per token of the text, in order: every
    ///   table's row concatenated, as a row pack stores it.
    ///
    /// # Returns
    ///
    /// The text's vector before normalising: the graph's output, averaged
    /// over the tokens when the model pools by mean.
    ///
    /// # Errors
    ///
    /// [`EngineError::Rows`] for no rows or rows of the wrong length,
    /// [`EngineError::Run`] when ONNX Runtime fails, and
    /// [`EngineError::OutputShape`] when the output is not what the model
    /// description promises.
    pub fn run(&self, rows: &[&[u8]]) -> Result<Vec<f32>, EngineError> {
        let tokens = rows.len();
        let token_count = i64::try_from(tokens).map_err(|_| EngineError::Rows)?;
        if tokens == 0 {
            return Err(EngineError::Rows);
        }
        let run = |err: ort::Error| EngineError::Run(err.to_string());
        let mut inputs: Vec<(String, SessionInputValue<'_>)> = Vec::new();
        let positions: Vec<i64> = (0..token_count).collect();
        inputs.push((
            self.ids_input.clone(),
            Tensor::from_array(([1, tokens], positions))
                .map_err(run)?
                .into(),
        ));
        inputs.push((
            self.mask_input.clone(),
            Tensor::from_array(([1, tokens], vec![1i64; tokens]))
                .map_err(run)?
                .into(),
        ));
        let mut offset = 0usize;
        for table in &self.tables {
            let width = usize::try_from(table.width).map_err(|_| EngineError::Rows)?;
            let bytes = table.row_bytes().ok_or(EngineError::Rows)?;
            let end = offset.checked_add(bytes).ok_or(EngineError::Rows)?;
            let mut column = Vec::with_capacity(tokens.saturating_mul(bytes));
            for row in rows {
                column.extend_from_slice(row.get(offset..end).ok_or(EngineError::Rows)?);
            }
            offset = end;
            let shape = [tokens, width];
            let value: SessionInputValue<'_> = match table.element {
                Element::U8 => Tensor::from_array((shape, column)).map_err(run)?.into(),
                Element::I8 => {
                    let signed: Vec<i8> = column
                        .into_iter()
                        .map(|byte| i8::from_le_bytes([byte]))
                        .collect();
                    Tensor::from_array((shape, signed)).map_err(run)?.into()
                }
                Element::F32 => {
                    let floats: Vec<f32> = column
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|chunk| f32::from_le_bytes(*chunk))
                        .collect();
                    Tensor::from_array((shape, floats)).map_err(run)?.into()
                }
            };
            inputs.push((table.input.clone(), value));
        }
        if rows.iter().any(|row| row.len() != offset) {
            return Err(EngineError::Rows);
        }

        let mut session = self.session.lock().unwrap_or_else(PoisonError::into_inner);
        let outputs = session.run(inputs).map_err(run)?;
        let (_, numbers) = outputs
            .get(self.output.as_str())
            .ok_or_else(|| EngineError::MissingName {
                kind: "output",
                name: self.output.clone(),
            })?
            .try_extract_tensor::<f32>()
            .map_err(run)?;
        match self.pooling {
            Pooling::None => {
                if numbers.len() != self.dimensions {
                    return Err(EngineError::OutputShape {
                        expected: self.dimensions,
                        found: numbers.len(),
                    });
                }
                Ok(numbers.to_vec())
            }
            Pooling::Mean => {
                let expected = self.dimensions.saturating_mul(tokens);
                if numbers.len() != expected {
                    return Err(EngineError::OutputShape {
                        expected,
                        found: numbers.len(),
                    });
                }
                let mut sums = vec![0f64; self.dimensions];
                for token in numbers.chunks_exact(self.dimensions) {
                    for (sum, number) in sums.iter_mut().zip(token) {
                        *sum += f64::from(*number);
                    }
                }
                // `tokens` is at most the model's token limit, far inside
                // the integers an f64 holds exactly.
                #[allow(clippy::cast_precision_loss)]
                let count = tokens as f64;
                // Narrowing the mean back to the graph's own precision.
                #[allow(clippy::cast_possible_truncation)]
                let mean = sums.into_iter().map(|sum| (sum / count) as f32).collect();
                Ok(mean)
            }
        }
    }
}
