using System.Diagnostics;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;
using Microsoft.TestPlatform.VsTestConsole.TranslationLayer;
using Microsoft.VisualStudio.TestPlatform.ObjectModel;
using Microsoft.VisualStudio.TestPlatform.ObjectModel.Client;
using Microsoft.VisualStudio.TestPlatform.ObjectModel.Logging;
using Microsoft.VisualStudio.TestPlatform.VsTestConsole.TranslationLayer.Interfaces;

namespace Dotest.ChurnSidecar;

internal static class Program
{
	private const string BaseOutputPathProperty = "-p:BaseOutputPath=bin/dotest/";
	private const string Divider = "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━";
	private const string EmptyRunSettingsXml = "<RunSettings></RunSettings>";

	private static readonly JsonSerializerOptions JsonOptions = new()
	{
		PropertyNameCaseInsensitive = true,
	};

	public static async Task<int> Main(string[] args)
	{
		ConfigureConsole();

		VsTestConsoleWrapper? consoleWrapper = null;
		ITestSession? testSession = null;

		try
		{
			var request = await ReadRequestAsync(args);
			if (request is null)
			{
				Console.WriteLine("✗ Missing churn request.");
				return 2;
			}

			if (request.TestNames.Count == 0)
			{
				Console.WriteLine("✗ Churn request did not include any selected tests.");
				return 2;
			}

			var projectPath = ResolveProjectPath(request.RepoRoot, request.TargetPath);
			if (!File.Exists(projectPath))
			{
				Console.WriteLine($"✗ Selected test project was not found: {projectPath}");
				return 2;
			}

			var testAssemblyPath = ResolveTargetPath(projectPath);

			if (!request.NoBuild)
			{
				Console.WriteLine("  Building selected test project once before persistent churn...");
				BuildProject(projectPath, request.NoRestore);
			}
			else if (!File.Exists(testAssemblyPath))
			{
				Console.WriteLine(
					"✗ The selected test assembly was not found. Disable no-build or build the project first."
				);
				return 2;
			}

			if (!File.Exists(testAssemblyPath))
			{
				Console.WriteLine($"✗ The selected test assembly was not found at {testAssemblyPath}");
				return 2;
			}

			var vstestConsolePath = FindVstestConsolePath();
			consoleWrapper = new VsTestConsoleWrapper(vstestConsolePath);
			consoleWrapper.StartSession();

			var sessionHandler = new SessionEventsHandler();
			testSession = consoleWrapper.StartTestSession(
				new[] { testAssemblyPath },
				EmptyRunSettingsXml,
				sessionHandler
			);

			if (testSession is null)
			{
				Console.WriteLine("✗ Could not start a persistent VSTest session.");
				return 2;
			}

			var selectedTestCases = ResolveSelectedTestCases(testSession, testAssemblyPath, request);
			return RunChurnLoop(testSession, testAssemblyPath, request, selectedTestCases);
		}
		catch (Exception ex)
		{
			Console.WriteLine($"✗ Persistent churn sidecar failed: {ex.Message}");
			return 2;
		}
		finally
		{
			if (testSession is not null)
			{
				try
				{
					testSession.StopTestSession();
				}
				catch
				{
				}
			}

			if (consoleWrapper is not null)
			{
				try
				{
					consoleWrapper.EndSession();
				}
				catch
				{
				}
			}
		}
	}

	private static async Task<ChurnRequest?> ReadRequestAsync(string[] args)
	{
		var requestFile = TryGetRequestFile(args);
		if (!string.IsNullOrWhiteSpace(requestFile))
		{
			var payload = await File.ReadAllTextAsync(requestFile, Encoding.UTF8);
			TryDeleteRequestFile(requestFile);
			return JsonSerializer.Deserialize<ChurnRequest>(payload, JsonOptions);
		}

		using var reader = new StreamReader(Console.OpenStandardInput(), Encoding.UTF8);
		var stdinPayload = await reader.ReadToEndAsync();
		if (string.IsNullOrWhiteSpace(stdinPayload))
		{
			return null;
		}

		return JsonSerializer.Deserialize<ChurnRequest>(stdinPayload, JsonOptions);
	}

	private static string? TryGetRequestFile(string[] args)
	{
		for (var index = 0; index + 1 < args.Length; index += 1)
		{
			if (string.Equals(args[index], "--request-file", StringComparison.Ordinal))
			{
				return args[index + 1];
			}
		}

		return null;
	}

	private static void TryDeleteRequestFile(string requestFile)
	{
		try
		{
			File.Delete(requestFile);
		}
		catch
		{
		}
	}

	private static int RunChurnLoop(
		ITestSession testSession,
		string testAssemblyPath,
		ChurnRequest request,
		IReadOnlyList<TestCase> selectedTestCases
	)
	{
		var iteration = 1;
		var successfulIterations = 0;
		var durations = new List<TimeSpan>();
		var useExactTestCases = selectedTestCases.Count > 0;

		if (!useExactTestCases && !string.IsNullOrWhiteSpace(request.Filter))
		{
			Console.WriteLine(
				"⚠ Falling back to filter-based churn because the exact selected test cases could not be resolved."
			);
		}

		while (true)
		{
			if (iteration > 1)
			{
				Console.WriteLine($"Iteration {iteration}   ↻ Starting");
			}

			var runHandler = new RunEventsHandler();
			var stopwatch = Stopwatch.StartNew();
			try
			{
				if (useExactTestCases)
				{
					testSession.RunTests(selectedTestCases, EmptyRunSettingsXml, runHandler);
				}
				else
				{
					var options = new TestPlatformOptions();
					if (!string.IsNullOrWhiteSpace(request.Filter))
					{
						options.TestCaseFilter = request.Filter;
					}

					testSession.RunTests(
						new[] { testAssemblyPath },
						EmptyRunSettingsXml,
						options,
						runHandler
					);
				}
			}
			catch (Exception ex)
			{
				stopwatch.Stop();
				durations.Add(stopwatch.Elapsed);
				Console.WriteLine($"✗ Sidecar test run failed: {ex.Message}");
				Console.WriteLine($"Iteration {iteration}   ✗ Failed ({FormatElapsed(stopwatch.Elapsed)})");
				Console.WriteLine();
				EmitFailureSummary(iteration, successfulIterations, durations, 2);
				return 2;
			}

			stopwatch.Stop();
			durations.Add(stopwatch.Elapsed);

			EmitRunCountSummary(runHandler);

			if (runHandler.Total == 0)
			{
				Console.WriteLine("✗ No tests matched the persistent churn selection.");
				Console.WriteLine($"Iteration {iteration}   ✗ Failed ({FormatElapsed(stopwatch.Elapsed)})");
				Console.WriteLine();
				EmitFailureSummary(iteration, successfulIterations, durations, 2);
				return 2;
			}

			if (runHandler.ExitCode == 0)
			{
				successfulIterations += 1;
				Console.WriteLine($"Iteration {iteration}   ✓ Passed ({FormatElapsed(stopwatch.Elapsed)})");

				if (request.IterationLimit is int limit && successfulIterations >= limit)
				{
					Console.WriteLine();
					EmitSuccessSummary(limit, successfulIterations, durations);
					return 0;
				}

				iteration += 1;
				continue;
			}

			Console.WriteLine($"Iteration {iteration}   ✗ Failed ({FormatElapsed(stopwatch.Elapsed)})");
			Console.WriteLine();
			EmitFailureSummary(iteration, successfulIterations, durations, runHandler.ExitCode);
			return runHandler.ExitCode;
		}
	}

	private static IReadOnlyList<TestCase> ResolveSelectedTestCases(
		ITestSession testSession,
		string testAssemblyPath,
		ChurnRequest request
	)
	{
		var discoveryHandler = new DiscoveryEventsHandler();
		testSession.DiscoverTests(new[] { testAssemblyPath }, EmptyRunSettingsXml, discoveryHandler);

		var discoveredCases = discoveryHandler.TestCases;
		if (discoveredCases.Count == 0)
		{
			return Array.Empty<TestCase>();
		}

		var requestedNames = request.TestNames
			.Select(NormalizeRequestedTestName)
			.Where(name => !string.IsNullOrWhiteSpace(name))
			.ToHashSet(StringComparer.Ordinal);

		if (requestedNames.Count == 0)
		{
			return Array.Empty<TestCase>();
		}

		var selectedCases = discoveredCases
			.Where(testCase => MatchesRequestedTest(testCase, requestedNames))
			.GroupBy(testCase => testCase.Id)
			.Select(group => group.First())
			.ToList();

		if (selectedCases.Count != 0)
		{
			return selectedCases;
		}

		return Array.Empty<TestCase>();
	}

	private static bool MatchesRequestedTest(TestCase testCase, HashSet<string> requestedNames)
	{
		var fullyQualifiedName = NormalizeRequestedTestName(testCase.FullyQualifiedName);
		if (requestedNames.Contains(fullyQualifiedName))
		{
			return true;
		}

		var displayName = NormalizeRequestedTestName(testCase.DisplayName);
		return !string.IsNullOrWhiteSpace(displayName) && requestedNames.Contains(displayName);
	}

	private static string NormalizeRequestedTestName(string? testName)
	{
		if (string.IsNullOrWhiteSpace(testName))
		{
			return string.Empty;
		}

		var trimmed = testName.Trim();
		var parameterIndex = trimmed.IndexOf('(');
		return parameterIndex >= 0 ? trimmed[..parameterIndex].Trim() : trimmed;
	}

	private static void EmitRunCountSummary(RunEventsHandler runHandler)
	{
		Console.WriteLine($"Passed: {runHandler.Passed}");
		Console.WriteLine($"Failed: {runHandler.Failed}");
		Console.WriteLine($"Skipped: {runHandler.Skipped}");
	}

	private static void EmitSuccessSummary(int limit, int successfulIterations, IReadOnlyList<TimeSpan> durations)
	{
		Console.WriteLine(Divider);
		Console.WriteLine($"  Churn completed: reached iteration limit {limit} with no failures.");
		Console.WriteLine($"  Successful iterations: {successfulIterations}");
		if (TryFormatDurationStats(durations, out var statsLine))
		{
			Console.WriteLine(statsLine);
		}
		Console.WriteLine(Divider);
	}

	private static void EmitFailureSummary(
		int iteration,
		int successfulIterations,
		IReadOnlyList<TimeSpan> durations,
		int exitCode
	)
	{
		Console.WriteLine(Divider);
		Console.WriteLine($"  Churn stopped on failure at iteration {iteration}.");
		Console.WriteLine($"  Successful iterations before failure: {successfulIterations}");
		if (TryFormatDurationStats(durations, out var statsLine))
		{
			Console.WriteLine(statsLine);
		}
		Console.WriteLine($"  Last run exit code: {exitCode}");
		Console.WriteLine(Divider);
	}

	private static bool TryFormatDurationStats(
		IReadOnlyList<TimeSpan> durations,
		out string statsLine
	)
	{
		if (durations.Count == 0)
		{
			statsLine = string.Empty;
			return false;
		}

		var min = durations.Min();
		var max = durations.Max();
		var averageTicks = (long)durations.Average(duration => duration.Ticks);
		var average = TimeSpan.FromTicks(averageTicks);

		statsLine =
			$"  Duration stats: avg {FormatElapsed(average)}  |  min {FormatElapsed(min)}  |  max {FormatElapsed(max)}";
		return true;
	}

	private static string ResolveProjectPath(string repoRoot, string targetPath)
	{
		var normalizedTarget = targetPath.Replace('/', Path.DirectorySeparatorChar);
		return Path.GetFullPath(Path.Combine(repoRoot, normalizedTarget));
	}

	private static string ResolveTargetPath(string projectPath)
	{
		var output = RunDotnetForOutput(
			"msbuild",
			projectPath,
			"-nologo",
			"-getProperty:TargetPath",
			BaseOutputPathProperty
		);

		var trimmedOutput = output.Trim();
		string? targetPath;
		if (trimmedOutput.StartsWith('{'))
		{
			using var document = JsonDocument.Parse(trimmedOutput);
			targetPath = document.RootElement
				.GetProperty("Properties")
				.GetProperty("TargetPath")
				.GetString();
		}
		else
		{
			targetPath = trimmedOutput
				.Split(new[] { '\r', '\n' }, StringSplitOptions.RemoveEmptyEntries)
				.FirstOrDefault();
		}

		if (string.IsNullOrWhiteSpace(targetPath))
		{
			throw new InvalidOperationException("dotnet msbuild did not return a TargetPath.");
		}

		return Path.GetFullPath(targetPath);
	}

	private static void BuildProject(string projectPath, bool noRestore)
	{
		var startInfo = CreateDotnetStartInfo(
			"build",
			projectPath,
			"-nologo",
			"-v",
			"q",
			BaseOutputPathProperty
		);
		if (noRestore)
		{
			startInfo.ArgumentList.Add("--no-restore");
		}

		using var process = Process.Start(startInfo)
			?? throw new InvalidOperationException("Could not start dotnet build.");
		process.WaitForExit();

		if (process.ExitCode != 0)
		{
			throw new InvalidOperationException($"dotnet build failed with exit code {process.ExitCode}.");
		}
	}

	private static string FindVstestConsolePath()
	{
		var roots = new[]
			{
				Environment.GetEnvironmentVariable("DOTNET_ROOT"),
				Path.Combine(
					Environment.GetFolderPath(Environment.SpecialFolder.ProgramFiles),
					"dotnet"
				),
				Path.Combine(
					Environment.GetFolderPath(Environment.SpecialFolder.ProgramFilesX86),
					"dotnet"
				),
			}
			.Where(path => !string.IsNullOrWhiteSpace(path) && Directory.Exists(path))
			.Distinct(StringComparer.OrdinalIgnoreCase);

		var candidates = new List<(Version Version, bool IsPrerelease, string Path)>();

		foreach (var root in roots)
		{
			var sdkDirectory = Path.Combine(root!, "sdk");
			if (!Directory.Exists(sdkDirectory))
			{
				continue;
			}

			foreach (var sdkPath in Directory.EnumerateDirectories(sdkDirectory))
			{
				var vstestPath = Path.Combine(sdkPath, "vstest.console.dll");
				if (!File.Exists(vstestPath))
				{
					continue;
				}

				var directoryName = Path.GetFileName(sdkPath);
				var stablePart = directoryName.Split('-', 2)[0];
				if (!Version.TryParse(stablePart, out var version))
				{
					version = new Version(0, 0);
				}

				candidates.Add((version, directoryName.Contains('-'), vstestPath));
			}
		}

		var best = candidates
			.OrderByDescending(candidate => candidate.Version)
			.ThenBy(candidate => candidate.IsPrerelease)
			.FirstOrDefault();

		if (string.IsNullOrWhiteSpace(best.Path))
		{
			throw new InvalidOperationException("Could not locate vstest.console.dll in the installed .NET SDK.");
		}

		return best.Path;
	}

	private static string RunDotnetForOutput(params string[] arguments)
	{
		var startInfo = CreateDotnetStartInfo(arguments);
		startInfo.RedirectStandardOutput = true;
		startInfo.RedirectStandardError = true;

		using var process = Process.Start(startInfo)
			?? throw new InvalidOperationException("Could not start dotnet.");
		var stdout = process.StandardOutput.ReadToEnd();
		var stderr = process.StandardError.ReadToEnd();
		process.WaitForExit();

		if (process.ExitCode != 0)
		{
			var message = string.IsNullOrWhiteSpace(stderr) ? stdout : stderr;
			throw new InvalidOperationException(message.Trim());
		}

		return stdout;
	}

	private static ProcessStartInfo CreateDotnetStartInfo(params string[] arguments)
	{
		var startInfo = new ProcessStartInfo("dotnet")
		{
			UseShellExecute = false,
		};

		foreach (var argument in arguments)
		{
			startInfo.ArgumentList.Add(argument);
		}

		return startInfo;
	}

	private static string FormatElapsed(TimeSpan duration)
	{
		var totalSeconds = Math.Max(0, (int)duration.TotalSeconds);
		return $"{totalSeconds / 60}:{totalSeconds % 60:00}";
	}

	private static void ConfigureConsole()
	{
		Console.OutputEncoding = Encoding.UTF8;
		Console.InputEncoding = Encoding.UTF8;

		var stdout = new StreamWriter(Console.OpenStandardOutput())
		{
			AutoFlush = true,
		};
		var stderr = new StreamWriter(Console.OpenStandardError())
		{
			AutoFlush = true,
		};

		Console.SetOut(stdout);
		Console.SetError(stderr);
	}
}

internal sealed class ChurnRequest
{
	[JsonPropertyName("repo_root")]
	public required string RepoRoot { get; init; }

	[JsonPropertyName("target_path")]
	public required string TargetPath { get; init; }

	[JsonPropertyName("filter")]
	public string? Filter { get; init; }

	[JsonPropertyName("test_names")]
	public List<string> TestNames { get; init; } = new();

	[JsonPropertyName("iteration_limit")]
	public int? IterationLimit { get; init; }

	[JsonPropertyName("no_build")]
	public bool NoBuild { get; init; }

	[JsonPropertyName("no_restore")]
	public bool NoRestore { get; init; }
}

internal sealed class RunEventsHandler : ITestRunEventsHandler
{
	private readonly HashSet<string> seenResults = new(StringComparer.Ordinal);

	public int Passed { get; private set; }
	public int Failed { get; private set; }
	public int Skipped { get; private set; }
	public int Total => Passed + Failed + Skipped;
	public int ExitCode { get; private set; }

	public void HandleTestRunComplete(
		TestRunCompleteEventArgs testRunCompleteArgs,
		TestRunChangedEventArgs? lastChunkArgs,
		ICollection<AttachmentSet>? runContextAttachments,
		ICollection<string>? executorUris
	)
	{
		if (lastChunkArgs is not null)
		{
			ProcessResults(lastChunkArgs.NewTestResults);
		}

		ApplyStatistics(testRunCompleteArgs.TestRunStatistics);

		if (testRunCompleteArgs.Error is not null)
		{
			Console.WriteLine($"✗ {testRunCompleteArgs.Error.Message}");
			ExitCode = 2;
			return;
		}

		ExitCode = testRunCompleteArgs.IsCanceled || testRunCompleteArgs.IsAborted || Failed > 0
			? 1
			: 0;
	}

	public void HandleTestRunStatsChange(TestRunChangedEventArgs? testRunChangedArgs)
	{
		ProcessResults(testRunChangedArgs?.NewTestResults);
		ApplyStatistics(testRunChangedArgs?.TestRunStatistics);
	}

	public int LaunchProcessWithDebuggerAttached(TestProcessStartInfo testProcessStartInfo) => -1;

	public void HandleRawMessage(string rawMessage)
	{
	}

	public void HandleLogMessage(TestMessageLevel level, string? message)
	{
		if (level == TestMessageLevel.Informational || string.IsNullOrWhiteSpace(message))
		{
			return;
		}

		Console.WriteLine(message.TrimEnd());
	}

	private void ProcessResults(IEnumerable<TestResult>? results)
	{
		if (results is null)
		{
			return;
		}

		foreach (var result in results)
		{
			var resultKey = BuildResultKey(result);
			if (!seenResults.Add(resultKey))
			{
				continue;
			}

			EmitResult(result);
		}
	}

	private void ApplyStatistics(ITestRunStatistics? statistics)
	{
		if (statistics is null)
		{
			return;
		}

		Passed = checked((int)statistics[TestOutcome.Passed]);
		Failed = checked((int)statistics[TestOutcome.Failed]);
		Skipped = checked((int)(statistics[TestOutcome.Skipped] + statistics[TestOutcome.None]));
	}

	private static string BuildResultKey(TestResult result)
	{
		var name = result.TestCase?.FullyQualifiedName ?? result.DisplayName ?? string.Empty;
		return $"{name}|{result.Outcome}";
	}

	private static void EmitResult(TestResult result)
	{
		var name = result.TestCase?.FullyQualifiedName ?? result.DisplayName ?? "<unknown test>";
		var durationSuffix = result.Duration > TimeSpan.Zero
			? $" [{Math.Max(1, (int)Math.Round(result.Duration.TotalMilliseconds))} ms]"
			: string.Empty;

		switch (result.Outcome)
		{
			case TestOutcome.Passed:
				Console.WriteLine($"Passed {name}{durationSuffix}");
				break;
			case TestOutcome.Skipped:
			case TestOutcome.None:
			case TestOutcome.NotFound:
				Console.WriteLine($"Skipped {name}{durationSuffix}");
				break;
			default:
				Console.WriteLine($"Failed {name}{durationSuffix}");

				var messageLines = SplitLines(result.ErrorMessage).ToArray();
				if (messageLines.Length > 0)
				{
					Console.WriteLine($"  Error Message: {messageLines[0]}");
					foreach (var extraLine in messageLines.Skip(1))
					{
						Console.WriteLine($"    {extraLine}");
					}
				}

				var stackLines = SplitLines(result.ErrorStackTrace).ToArray();
				if (stackLines.Length > 0)
				{
					Console.WriteLine("  Stack Trace:");
					foreach (var stackLine in stackLines)
					{
						Console.WriteLine($"    {stackLine}");
					}
				}
				break;
		}
	}

	private static IEnumerable<string> SplitLines(string? value)
	{
		return string.IsNullOrWhiteSpace(value)
			? Array.Empty<string>()
			: value
				.Replace("\r\n", "\n", StringComparison.Ordinal)
				.Replace('\r', '\n')
				.Split('\n', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries);
	}
}

internal sealed class DiscoveryEventsHandler : ITestDiscoveryEventsHandler
{
	private readonly Dictionary<Guid, TestCase> seenCases = new();

	public IReadOnlyList<TestCase> TestCases => seenCases.Values.ToList();

	public void HandleDiscoveryComplete(
		long totalTests,
		IEnumerable<TestCase>? lastChunk,
		bool isAborted
	)
	{
		Record(lastChunk);
	}

	public void HandleDiscoveredTests(IEnumerable<TestCase>? discoveredTestCases)
	{
		Record(discoveredTestCases);
	}

	public void HandleRawMessage(string rawMessage)
	{
	}

	public void HandleLogMessage(TestMessageLevel level, string? message)
	{
		if (level == TestMessageLevel.Informational || string.IsNullOrWhiteSpace(message))
		{
			return;
		}

		Console.WriteLine(message.TrimEnd());
	}

	private void Record(IEnumerable<TestCase>? discoveredTestCases)
	{
		if (discoveredTestCases is null)
		{
			return;
		}

		foreach (var testCase in discoveredTestCases)
		{
			seenCases[testCase.Id] = testCase;
		}
	}
}

internal sealed class SessionEventsHandler : ITestSessionEventsHandler
{
	public void HandleStartTestSessionComplete(StartTestSessionCompleteEventArgs? eventArgs)
	{
	}

	public void HandleStopTestSessionComplete(StopTestSessionCompleteEventArgs? eventArgs)
	{
	}

	public void HandleRawMessage(string rawMessage)
	{
	}

	public void HandleLogMessage(TestMessageLevel level, string? message)
	{
		if (level == TestMessageLevel.Informational || string.IsNullOrWhiteSpace(message))
		{
			return;
		}

		Console.WriteLine(message.TrimEnd());
	}
}