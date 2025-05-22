from abc import ABC, abstractmethod
import argparse


class BaseCommand(ABC):
    """Base class for all Avocado commands."""

    @classmethod
    @abstractmethod
    def register_subparser(cls, subparsers):
        """
        Register the command's subparser.

        Args:
            subparsers: The subparsers object from argparse to add this command to.
        """
        pass

    @abstractmethod
    def execute(self, args, parser=None, unknown=None):
        """
        Execute the command with the provided arguments.

        Args:
            args: The parsed command-line arguments from argparse.
            parser: The parser object (optional).
            unknown: Unknown arguments that weren't parsed (optional).

        Returns:
            bool: True if the command executed successfully, False otherwise.
        """
        pass
