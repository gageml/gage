// A static picture of one exchange with a coding agent: the question
// typed, what the agent looked up, and the answer. The data for each
// exchange is a JSON file under pages/public/examples, named for the
// slug of the topic it illustrates.

export interface ExampleData {
  question: string;
  steps?: string[];
  answer: string[];
}

export async function loadExample(
  slug: string,
): Promise<ExampleData | undefined> {
  try {
    const response = await fetch(`/examples/${slug}.json`);
    if (!response.ok) return undefined;
    return (await response.json()) as ExampleData;
  } catch {
    return undefined;
  }
}

export function ExchangeWindow({ example }: { example?: ExampleData }) {
  return (
    <div className="gage-session">
      <div className="gage-session__bar">
        <span className="gage-session__dots" aria-hidden="true">
          <i />
          <i />
          <i />
        </span>
      </div>
      <div className="gage-session__body">
        {example && (
          <>
            <p className="gage-session__question">
              <span className="gage-session__prompt" aria-hidden="true">
                &gt;
              </span>
              {example.question}
            </p>
            {example.steps?.map((step) => (
              <p key={step} className="gage-session__step">
                <span className="gage-session__bullet" aria-hidden="true" />
                {step}
              </p>
            ))}
            <div className="gage-session__answer">
              {example.answer.map((paragraph) => (
                <p key={paragraph}>{paragraph}</p>
              ))}
            </div>
          </>
        )}
      </div>
    </div>
  );
}
