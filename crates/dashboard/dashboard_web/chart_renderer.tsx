import { Fragment } from "react";
import type { ChartData, ChartPlot, ChartSeries } from "./chart_data";
import { BOTTOM, LEFT, RIGHT, TOP, chartValueAtFraction, chartValueText, chartXFraction, chartYFraction, formatChartX, type ChartRange } from "./chart_math";

const COLORS = ["#008573", "#4b6fd8", "#cf7b23", "#9960bd", "#cf526c", "#5c8837", "#a46b16", "#277f9c"];

function runColor(run_index: number): string { return COLORS[run_index - 1]!; }
function valueTick(value: number): string {
  if (value === 0) return "0";
  return Math.abs(value) < .0001 || Math.abs(value) >= 100_000
    ? value.toExponential(3).replace("e+", "e") : value.toFixed(6).replace(/\.?0+$/, "");
}
function metricRange(data: ChartData, plot: ChartPlot): ChartRange | null {
  return plot.domain ? { x_axis: data.x_axis, start_x: plot.domain.x_min, end_x: plot.domain.x_max } : null;
}
function SeriesGlyphs({ series, plot, range }: { series: ChartSeries; plot: ChartPlot; range: ChartRange }) {
  const domain = plot.domain!;
  const points = series.points.map(point => ({
    ...point,
    x: LEFT + chartXFraction(point.x_value, range) * (RIGHT - LEFT),
    y: TOP + (1 - chartYFraction(point.value, domain.value_min, domain.value_max)) * (BOTTOM - TOP),
  }));
  return <Fragment>
    {points.length > 0 && <polyline
      data-chart-run-index={series.run_index}
      fill="none" stroke={runColor(series.run_index)} strokeWidth="2"
      points={points.map(point => `${point.x},${point.y}`).join(" ")}
    />}
    {points.map((point, index) => {
      const label = `Run ${series.run_index} · step ${point.step} · value ${chartValueText(point.value)}${point.timestamp ? ` · timestamp ${point.timestamp}` : ""}${range.x_axis === "elapsed" ? ` · elapsed ${formatChartX(point.x_value, "elapsed")}` : ""}`;
      return <circle key={index} className="point" cx={point.x} cy={point.y} r="2"
        fill={runColor(series.run_index)} aria-label={label}
        data-chart-run-index={series.run_index} data-chart-sample-index={index}
        data-x-value={point.x_value} data-timestamp={point.timestamp ?? undefined}
      ><title>{label}</title></circle>;
    })}
  </Fragment>;
}

function MetricPlot({ data, plot }: { data: ChartData; plot: ChartPlot }) {
  const range = metricRange(data, plot);
  const summary_by_run = new Map(plot.series.map(series => [series.run_index, series]));
  const sampled = plot.series.some(series => series.points.length < series.summary.count - (data.x_axis === "step" ? 0 : series.summary.missing_timestamp_count));
  const last = data.runs.length === 1 ? plot.series[0]?.summary.last : null;
  return <section className="metric-chart evidence-panel" data-chart-metric={plot.metric_name}>
    <header className="metric-chart-header">
      <h2 className="metric-chart-title">{plot.metric_name}</h2>
      {last && <div className="metric-chart-summary" title="Last recorded value across all samples">
        <span className="muted">Last</span>
        <strong title={chartValueText(last.value)}>{valueTick(last.value)}</strong>
      </div>}
    </header>
    {data.x_axis !== "step" && plot.series.filter(series => series.summary.missing_timestamp_count > 0).map(series => <p className="muted" key={series.run_index}>
      Run {series.run_index} · <code>{data.runs[series.run_index - 1]}</code>: {series.summary.missing_timestamp_count} of {series.summary.count} samples omitted from this time view because they have no timestamp.
    </p>)}
    {range && plot.domain ? <Fragment>
      <div className="plot-scroll" tabIndex={0} role="region" aria-label={`Plot of ${plot.metric_name}`}>
        <svg viewBox="-84 0 1084 340" role="img" aria-label={`${plot.metric_name} by ${data.x_axis === "step" ? "step" : data.x_axis === "elapsed" ? "elapsed time" : "date and time"}`}
          data-x-axis={data.x_axis} data-x-min={range.start_x} data-x-max={range.end_x}>
          <title>{plot.metric_name}</title>
          <desc>Samples remain in logging order. The legend lists complete sample counts; inspection shows exact recorded values.</desc>
          {Array.from({ length: plot.domain.value_min === plot.domain.value_max ? 1 : 5 }, (_, index) => {
            const fraction = plot.domain!.value_min === plot.domain!.value_max ? .5 : index / 4;
            const value = chartValueAtFraction(plot.domain!.value_min, plot.domain!.value_max, fraction);
            const y = TOP + (1 - fraction) * (BOTTOM - TOP);
            return <Fragment key={index}>
              <line className="grid" x1={LEFT} x2={RIGHT} y1={y} y2={y} />
              <text className="tick" x={LEFT - 12} y={y + 4} textAnchor="end"><title>{chartValueText(value)}</title>{valueTick(value)}</text>
            </Fragment>;
          })}
          <text className="axis-label" x={(LEFT + RIGHT) / 2} y={BOTTOM + 52} textAnchor="middle">
            {data.x_axis === "step" ? "Step" : data.x_axis === "elapsed" ? "Elapsed" : "Date & time"}
          </text>
          {plot.series.map(series => <SeriesGlyphs key={series.run_index} series={series} plot={plot} range={range} />)}
        </svg>
      </div>
      {data.x_axis === "wall_clock" && <p className="muted">Dates and times are shown in the selected timezone.</p>}
    </Fragment> : <p className="muted">{data.x_axis === "step" ? "No finite samples are available to plot." : "No timestamped samples are available to plot."}</p>}
    <ul className="legend">
      {data.runs.map((run_id, index) => {
        const series = summary_by_run.get(index + 1);
        return <li key={run_id} data-chart-run-index={index + 1}>
          <span className="swatch" />
          <span>Run {index + 1}</span><code>{run_id}</code>
          <span className="muted">{series ? `${series.summary.count} sample${series.summary.count === 1 ? "" : "s"}` : "not recorded"}</span>
        </li>;
      })}
    </ul>
    {sampled && <p className="muted">Curves are downsampled to at most 600 points per run, preserving endpoints and bucket minima/maxima. Sample counts include every logged sample.</p>}
  </section>;
}

export function ChartPreview({ data }: { data: ChartData }) {
  return <Fragment>
    <p className="chart-summary">{data.plots.length} metric{data.plots.length === 1 ? "" : "s"} · {data.runs.length} run{data.runs.length === 1 ? "" : "s"} · Logging order</p>
    {(data.warnings.length > 0 || data.warnings_truncated) && <section className="evidence-panel warning">
      <h2>Data notes</h2>
      <ul tabIndex={0} aria-label="Metric data notes">{data.warnings.map((warning, index) => <li key={index}><code>{data.runs[warning.run_index - 1]}</code>: {warning.message}</li>)}</ul>
      {data.warnings_truncated && <p className="muted">Some data notes are shortened or omitted. Review the original run records for complete notes.</p>}
    </section>}
    {data.plots.length === 0 && <section className="evidence-panel"><h2>No scalar metrics</h2><p className="muted">No metric series are available in these run records.</p></section>}
    {data.plots.map(plot => <MetricPlot key={plot.metric_name} data={data} plot={plot} />)}
    {data.runs.length > 1 && <details className="parameter-comparison"><summary>Compare parameters</summary>
      <section className="evidence-panel"><h2>Effective parameters</h2>
        {data.parameter_rows.length === 0 ? <p className="muted">No effective parameters were saved for these runs.</p> : <Fragment>
          <p className="muted">Highlighted rows differ between runs. A dash means the parameter was not recorded.</p>
          <div className="table-scroll"><table className="params"><thead><tr><th>Parameter</th>{data.runs.map(run_id => <th key={run_id}><code>{run_id}</code></th>)}</tr></thead>
            <tbody>{data.parameter_rows.map((row, index) => <tr key={index} className={row.differs ? "diff" : undefined}>
              <th scope="row"><code>{row.name}</code>{row.differs && <span className="diff-label">differs</span>}</th>
              {row.values.map((value, index) => <td key={index} className={value === null ? "muted" : undefined}>{value === null ? "—" : <pre>{value}</pre>}</td>)}
            </tr>)}</tbody></table></div>
        </Fragment>}
        {data.parameters_truncated && <p className="muted">Parameter names and values are limited to 1000 characters, with at most 24 rows. Highlighted rows compare complete saved values.</p>}
      </section>
    </details>}
  </Fragment>;
}
